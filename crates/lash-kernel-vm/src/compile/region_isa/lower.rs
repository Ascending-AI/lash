//! Lowering emits indexed operations; it never evaluates a source path.
use super::*;
use crate::compile::Form;
use crate::compile::room::charges::region_isa::{self as canonical};
use crate::compile::room::{CallSource, Owner};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Edge {
    Next(Point),
    Branch { input: Owner, yes: Point, no: Point },
    Cursor { body: Point, done: Point },
    Stop,
    Boundary,
}
pub(super) fn point(site: &Site, phase: Phase) -> Point {
    Point {
        site: site.clone(),
        phase,
    }
}

pub(super) fn connect(
    node: doc::Node<'_>,
    site: &Site,
    next: Option<Point>,
    source: &Source,
    graph: &mut BTreeMap<Point, Edge>,
) -> Result<Point, Refused> {
    let commit = point(site, Phase::Commit);
    if let doc::Node::Block(body) = node {
        let leave = point(site, Phase::Leave);
        graph.insert(leave.clone(), next.map_or(Edge::Stop, Edge::Next));
        let mut tail = leave;
        if site == &source.site {
            let fall = point(site, Phase::Fallthrough);
            graph.insert(fall.clone(), Edge::Next(tail));
            tail = fall;
        }
        for (index, statement) in body.iter().enumerate().rev() {
            tail = connect(
                doc::Node::Stmt(statement),
                &site.child(index as u32),
                Some(tail),
                source,
                graph,
            )?;
        }
        let enter = point(site, Phase::Enter);
        graph.insert(enter.clone(), Edge::Next(tail));
        return Ok(enter);
    }
    if let Some(entry) = super::call_paths::connect(node, site, next.clone(), source, graph)? {
        return Ok(entry);
    }
    graph.insert(commit.clone(), next.map_or(Edge::Stop, Edge::Next));
    Ok(match node {
        doc::Node::Stmt(doc::Stmt::While {
            condition,
            body,
            else_block,
            ..
        }) => {
            let test = point(site, Phase::Test);
            let condition_entry = connect(
                doc::Node::Expr(condition),
                &site.child(0),
                Some(test.clone()),
                source,
                graph,
            )?;
            let body_entry = connect(
                doc::Node::Block(body),
                &site.child(1),
                Some(condition_entry.clone()),
                source,
                graph,
            )?;
            let done = if else_block.is_empty() {
                commit
            } else {
                connect(
                    doc::Node::Block(else_block),
                    &site.child(2),
                    Some(commit),
                    source,
                    graph,
                )?
            };
            graph.insert(
                test,
                Edge::Branch {
                    input: Owner::Operand(site.child(0)),
                    yes: body_entry,
                    no: done,
                },
            );
            condition_entry
        }
        doc::Node::Stmt(doc::Stmt::For {
            iterable,
            body,
            else_block,
            via: None,
            ..
        }) => {
            let test = point(site, Phase::Test);
            let body_entry = connect(
                doc::Node::Block(body),
                &site.child(1),
                Some(test.clone()),
                source,
                graph,
            )?;
            let done = if else_block.is_empty() {
                commit
            } else {
                connect(
                    doc::Node::Block(else_block),
                    &site.child(2),
                    Some(commit),
                    source,
                    graph,
                )?
            };
            graph.insert(
                test.clone(),
                Edge::Cursor {
                    body: body_entry,
                    done,
                },
            );
            connect(
                doc::Node::Expr(iterable),
                &site.child(0),
                Some(test),
                source,
                graph,
            )?
        }
        doc::Node::Stmt(doc::Stmt::If {
            condition,
            then_block,
            else_block,
        }) => {
            let yes = connect(
                doc::Node::Block(then_block),
                &site.child(1),
                Some(commit.clone()),
                source,
                graph,
            )?;
            let no = connect(
                doc::Node::Block(else_block),
                &site.child(2),
                Some(commit),
                source,
                graph,
            )?;
            let test = point(site, Phase::Test);
            graph.insert(
                test.clone(),
                Edge::Branch {
                    input: Owner::Operand(site.child(0)),
                    yes,
                    no,
                },
            );
            connect(
                doc::Node::Expr(condition),
                &site.child(0),
                Some(test),
                source,
                graph,
            )?
        }
        // A conditional expression runs one arm; its Commit joins it.
        doc::Node::Expr(doc::Expr::If { cond, then, r#else }) => {
            let yes = connect(
                doc::Node::Expr(then),
                &site.child(1),
                Some(commit.clone()),
                source,
                graph,
            )?;
            let no = connect(
                doc::Node::Expr(r#else),
                &site.child(2),
                Some(commit),
                source,
                graph,
            )?;
            let test = point(site, Phase::Test);
            graph.insert(
                test.clone(),
                Edge::Branch {
                    input: Owner::Operand(site.child(0)),
                    yes,
                    no,
                },
            );
            connect(
                doc::Node::Expr(cond),
                &site.child(0),
                Some(test),
                source,
                graph,
            )?
        }
        // A let expression binds its value at Test, then runs its body.
        doc::Node::Expr(doc::Expr::Let { value, body, .. }) => {
            let body = connect(
                doc::Node::Expr(body),
                &site.child(1),
                Some(commit),
                source,
                graph,
            )?;
            let test = point(site, Phase::Test);
            graph.insert(test.clone(), Edge::Next(body));
            connect(
                doc::Node::Expr(value),
                &site.child(0),
                Some(test),
                source,
                graph,
            )?
        }
        doc::Node::Stmt(doc::Stmt::Return { value }) => {
            graph.insert(commit.clone(), Edge::Stop);
            connect(
                doc::Node::Expr(value),
                &site.child(0),
                Some(commit),
                source,
                graph,
            )?
        }
        // A loop departure has no operand; its route is appended with the
        // departures and ends at the target loop's Catch.
        doc::Node::Stmt(doc::Stmt::Break | doc::Stmt::Continue) => {
            graph.insert(commit.clone(), Edge::Stop);
            commit
        }
        // A statement action evaluates its atoms at Enter and delivers its
        // result to its Commit; a call's completion arrives there too.
        doc::Node::Action(_) => {
            let enter = point(site, Phase::Enter);
            graph.insert(enter.clone(), Edge::Next(commit));
            enter
        }
        // These require departure/handler or protocol composition. Each is an
        // explicit boundary, with no source interpreter embedded in the ISA.
        doc::Node::Stmt(
            doc::Stmt::ReturnVia { .. }
            | doc::Stmt::Throw { .. }
            | doc::Stmt::Finish { .. }
            | doc::Stmt::Fail { .. }
            | doc::Stmt::Try(_)
            | doc::Stmt::For { via: Some(_), .. },
        ) => {
            let first = source.first_event(site)?;
            graph.insert(first.clone(), Edge::Boundary);
            first
        }
        doc::Node::Expr(doc::Expr::Closure(_)) => commit,
        _ => {
            let mut tail = commit;
            let mut children: Vec<_> = node.children().into_iter().enumerate().collect();
            if matches!(
                node,
                doc::Node::Stmt(doc::Stmt::Assign {
                    place: doc::Place::Member(_),
                    order: doc::AssignOrder::ValueFirst,
                    ..
                })
            ) {
                children.rotate_right(1);
            }
            for (index, child) in children.into_iter().rev() {
                tail = connect(child, &site.child(index as u32), Some(tail), source, graph)?;
            }
            tail
        }
    })
}

pub(super) fn graph(source: &Source) -> Result<(Point, BTreeMap<Point, Edge>), Refused> {
    let mut graph = BTreeMap::new();
    let entry = connect(
        doc::Node::Block(&source.body),
        &source.site,
        None,
        source,
        &mut graph,
    )?;
    Ok((entry, graph))
}

pub(super) fn origins(
    exe: &Executable,
    code: CodeId,
) -> Result<(BTreeMap<Point, KirOrigin>, canonical::Program), Refused> {
    let mut origins = BTreeMap::new();
    let mut blocks = vec![exe.code(code).body];
    let mut seen = BTreeSet::new();
    while let Some(block) = blocks.pop() {
        if !seen.insert(block) {
            continue;
        }
        for statement in &exe.block(block).stmts {
            let (stream, stmt, _) = exe.statement(*statement);
            blocks.extend(stmt.form.blocks());
            let ranges = match stmt.form {
                Form::While { test, .. } => vec![stmt.ops, test],
                _ => vec![stmt.ops],
            };
            for range in &ranges {
                if !stmt.verify_holding_sites(stream, &exe.code(code).source_room, *range) {
                    return Err(Refused::Source);
                }
                for pc in range.start..range.end() {
                    let op = *stream.ops.get(pc as usize).ok_or(Refused::Path)?;
                    let Some(site) = stream
                        .source_sites
                        .get(pc as usize)
                        .and_then(Option::as_ref)
                    else {
                        continue;
                    };
                    if matches!(op, Op::Region { .. } | Op::Pin { .. } | Op::End | Op::While) {
                        continue;
                    }
                    let phase = match op {
                        Op::Test { .. }
                        | Op::Branch { .. }
                        | Op::LetBind { .. }
                        | Op::LetBindShared { .. } => Phase::Test,
                        Op::Act => Phase::Enter,
                        // Every nested action but a library body enters at its
                        // Enter: a call, or a delivering action's boundary.
                        Op::Nested { act, .. }
                            if stream.nested.get(act.ix()).is_some_and(|entry| {
                                !matches!(entry.kind, super::super::NestedKind::Body(_))
                            }) =>
                        {
                            Phase::Enter
                        }
                        _ => Phase::Commit,
                    };
                    let at = point(site, phase);
                    let origin = KirOrigin {
                        statement: *statement,
                        pc,
                        op,
                    };
                    // A real semantic opcode owns the point, not a following
                    // source-control marker at the same physical site.
                    origins.entry(at).or_insert(origin);
                }
            }
        }
    }
    Ok((origins, canonical::program(exe, code)?))
}

pub(super) fn literal(literal: &doc::Literal) -> Option<Value> {
    Some(match literal {
        doc::Literal::Null => Value::Null,
        doc::Literal::Absent => Value::Absent,
        doc::Literal::Bool(value) => Value::Bool(*value),
        doc::Literal::Int(value) => Value::Int(value.word()?),
        doc::Literal::Float(value) => Value::Float(*value),
        doc::Literal::Text(value) => Value::Text(doc::arena::ShortText::new(value)?),
        _ => return None,
    })
}
pub(super) fn node_at<'s>(source: &'s Source, at: &Point) -> Option<doc::Node<'s>> {
    resources::node(source, &at.site).ok()
}
pub(super) fn value_literal(source: &Source, at: &Point) -> Option<Value> {
    match resources::node(source, &at.site).ok()? {
        doc::Node::Expr(doc::Expr::Literal(value)) if at.phase == Phase::Commit => literal(value),
        doc::Node::Block(_) if at.phase == Phase::Fallthrough => Some(Value::Null),
        _ => None,
    }
}
pub(super) fn reg(
    roles: &[(Role, Reg<LeaseValue>)],
    source: &Source,
    owner: &Owner,
) -> Result<Reg<LeaseValue>, Refused> {
    if matches!(owner, Owner::Incoming(_))
        && source
            .owner_slots()?
            .root(owner, doc::TaskId::MAIN, 0)
            .is_some()
    {
        return Reg::new(roles.len());
    }
    let slot = resources::role(source, owner).ok_or(Refused::Facts)?;
    roles
        .iter()
        .find(|(candidate, _)| *candidate == slot)
        .map(|(_, reg)| *reg)
        .ok_or(Refused::Facts)
}

pub(super) fn compile(exe: &Executable, code: CodeId) -> Result<RegionDraft, Refused> {
    if code.0 as usize >= exe.code_count() {
        return Err(Refused::Source);
    }
    let source = Arc::clone(&exe.code(code).source_room);
    source.verify()?;
    let roles = resources::roles(&source)?;
    let (entry, graph) = graph(&source)?;
    let (origins, program) = origins(exe, code)?;
    let closes = &program.events;
    let points: Vec<_> = source.room.events.keys().cloned().collect();
    let pcs: BTreeMap<_, _> = points
        .iter()
        .enumerate()
        .map(|(index, point)| {
            Ok((
                point.clone(),
                Pc(u32::try_from(index).map_err(|_| Refused::Facts)?),
            ))
        })
        .collect::<Result<_, Refused>>()?;
    let pc = |at: &Point| pcs.get(at).copied().ok_or(Refused::Path);
    u32::try_from(points.len().checked_mul(3).ok_or(Refused::Facts)?)
        .map_err(|_| Refused::Facts)?;
    let mut registers = vec![RegisterKind::Value; roles.len()];
    let sink = Reg::new(registers.len())?;
    registers.push(RegisterKind::Value);
    let mut ops = Vec::new();
    let mut steps = Vec::new();
    let mut calls = Vec::new();
    for at in &points {
        // A conditional Commit's base row joins its then arm.
        let event = if super::joins::joined(&source, at)? {
            super::joins::event(&source, at, 1)?
        } else {
            source.room.events.get(at).ok_or(Refused::Source)?.clone()
        };
        let node = resources::node(&source, &at.site)?;
        let edge = graph.get(at).unwrap_or(&Edge::Stop);
        let next = match edge {
            Edge::Next(at) => Some(pc(at)?),
            _ => None,
        };
        let output = event
            .output
            .as_ref()
            .and_then(|owner| reg(&roles, &source, owner).ok());
        let inputs = event
            .inputs
            .iter()
            .map(|owner| reg(&roles, &source, owner))
            .collect::<Result<Box<[_]>, _>>();
        let origin = origins.get(at).copied();
        let mut depth = 0;
        let mut scalar_resources = false;
        let op = match edge {
            Edge::Boundary => RegionOp::Boundary {
                spec: BoundarySpec {
                    action: None,
                    at: at.clone(),
                    origin,
                    next: None,
                },
            },
            Edge::Branch { input, yes, no } => {
                let input = reg(&roles, &source, input)?;
                // Branch itself performs the canonical checked unpack at Test.
                // The typed destination is private and is never an unchecked cast.
                let test = Reg::<bool>::new(registers.len())?;
                registers.push(RegisterKind::Bool);
                let guard = CheckedKernel {
                    kind: CheckedKind::Truth {
                        input,
                        output: test,
                    },
                };
                let before = PrefixId(
                    u32::try_from(steps.len().checked_mul(3).ok_or(Refused::Facts)?)
                        .map_err(|_| Refused::Facts)?,
                );
                steps.push(Step {
                    at: at.clone(),
                    peak: IndexedAmount::compile(
                        &source,
                        &roles,
                        &event.peak,
                        event.output.as_ref(),
                    )?,
                    delta: IndexedAmount::compile(
                        &source,
                        &roles,
                        &event.delta,
                        event.output.as_ref(),
                    )?,
                    event,
                    before,
                    pending: PrefixId(before.0 + 1),
                    after: PrefixId(before.0 + 2),
                    charges: closes.get(at).cloned().unwrap_or_default().into(),
                    depth: 0,
                    scalar_resources: true,
                    reachable: false,
                    stage: None,
                    scratch: source
                        .room
                        .scratch
                        .iter()
                        .filter(|scratch| scratch.at == *at)
                        .map(|scratch| {
                            IndexedAmount::compile(&source, &roles, &scratch.amount, None)
                        })
                        .collect::<Result<_, _>>()?,
                    guard: Some(guard),
                });
                ops.push(RegionOp::Branch {
                    test,
                    yes: pc(yes)?,
                    no: pc(no)?,
                });
                continue;
            }
            Edge::Cursor { body, done } => {
                let dst = output.unwrap_or(sink);
                RegionOp::Checked {
                    kernel: CheckedKernel {
                        kind: CheckedKind::Cursor {
                            at: at.site.clone(),
                            exhausted: pc(done)?,
                        },
                    },
                    args: Box::new([]),
                    dst,
                    next: pc(body)?,
                }
            }
            _ if value_literal(&source, at).is_some() && next.is_some() && output.is_some() => {
                let value = value_literal(&source, at).ok_or(Refused::Facts)?;
                let dst = output.ok_or(Refused::Facts)?;
                registers[dst.offset()] = match value {
                    Value::Int(_) => RegisterKind::Int,
                    Value::Bool(_) => RegisterKind::Bool,
                    Value::Float(_) => RegisterKind::Float,
                    _ => RegisterKind::Value,
                };
                scalar_resources = true;
                RegionOp::Constant {
                    dst,
                    value,
                    next: next.ok_or(Refused::Path)?,
                }
            }
            _ => match (node, at.phase, next, output, inputs) {
                (doc::Node::Stmt(doc::Stmt::Return { .. }), Phase::Commit, None, _, Ok(args))
                    if args.len() == 1 =>
                {
                    RegionOp::Return { value: args[0] }
                }
                // The route is appended below; its first Leave replaces `next`.
                (
                    doc::Node::Stmt(doc::Stmt::Break | doc::Stmt::Continue),
                    Phase::Commit,
                    None,
                    _,
                    Ok(args),
                ) if args.is_empty() && event.entities.is_empty() => RegionOp::Checked {
                    kernel: CheckedKernel {
                        kind: CheckedKind::Phase,
                    },
                    args: Box::new([]),
                    dst: sink,
                    next: pc(at)?,
                },
                (
                    doc::Node::Expr(doc::Expr::Call { .. } | doc::Expr::Act(_)),
                    Phase::Restore,
                    Some(next),
                    Some(dst),
                    Ok(args),
                ) if args.len() == 1 => RegionOp::Move {
                    dst,
                    src: args[0],
                    next,
                },
                (doc::Node::Expr(doc::Expr::Act(act)), Phase::Open, Some(next), Some(dst), _)
                    if matches!(
                        act.kind,
                        doc::ActKind::Call(doc::Callee::Value(_))
                            | doc::ActKind::Spawn(doc::Callee::Value(_))
                    ) =>
                {
                    let (doc::ActKind::Call(doc::Callee::Value(name))
                    | doc::ActKind::Spawn(doc::Callee::Value(name))) = &act.kind
                    else {
                        return Err(Refused::Source);
                    };
                    RegionOp::Checked {
                        kernel: CheckedKernel {
                            kind: CheckedKind::Local {
                                slot: resources::role(
                                    &source,
                                    &Owner::Visible {
                                        at: at.site.clone(),
                                        name: name.clone(),
                                    },
                                )
                                .ok_or(Refused::Facts)?,
                            },
                        },
                        args: Box::new([]),
                        dst,
                        next,
                    }
                }
                // A delivering action's Enter stops after its acceptance; the
                // machine performs it and resumes at its Commit, the result
                // the task's Incoming in the sink.
                (doc::Node::Expr(doc::Expr::Act(act)), Phase::Enter, Some(next), None, Ok(_))
                    if !matches!(act.kind, doc::ActKind::Call(_)) =>
                {
                    let commit = source
                        .room
                        .events
                        .get(&point(&at.site, Phase::Commit))
                        .ok_or(Refused::Source)?;
                    RegionOp::Boundary {
                        spec: BoundarySpec {
                            at: at.clone(),
                            action: Some(ActionBoundary {
                                atoms: Box::new([]),
                                arguments: commit
                                    .inputs
                                    .iter()
                                    .filter(|owner| !matches!(owner, Owner::Incoming(_)))
                                    .map(|owner| reg(&roles, &source, owner))
                                    .collect::<Result<_, _>>()?,
                            }),
                            origin: Some(origin.ok_or(Refused::Source)?),
                            next: Some(next),
                        },
                    }
                }
                // A statement call of a guest body loads its atoms, which
                // the caller holds until the call's Commit takes its result.
                (
                    doc::Node::Action(
                        action @ (doc::Action::Call { .. } | doc::Action::Await { .. }),
                    ),
                    Phase::Enter,
                    Some(next),
                    None,
                    Ok(_),
                ) if origin.is_some_and(|origin| {
                    CallPlan::compile(exe, code, origin.statement, origin.pc).is_ok()
                }) =>
                {
                    let atoms =
                        action_atoms(&roles, &source, &at.site, action).ok_or(Refused::Facts)?;
                    let site = CallId(u32::try_from(calls.len()).map_err(|_| Refused::Facts)?);
                    calls.push(CallSite {
                        at: at.clone(),
                        origin: origin.ok_or(Refused::Source)?,
                        arguments: atoms.iter().map(|atom| atom.dst).collect(),
                        atoms,
                        result: sink,
                    });
                    RegionOp::Call { site, next }
                }
                (doc::Node::Action(action), Phase::Enter, Some(next), None, Ok(_))
                    if !matches!(action, doc::Action::Call { .. } | doc::Action::Await { .. }) =>
                {
                    match action_atoms(&roles, &source, &at.site, action) {
                        Some(atoms) => RegionOp::Boundary {
                            spec: BoundarySpec {
                                at: at.clone(),
                                action: Some(ActionBoundary {
                                    arguments: atoms.iter().map(|atom| atom.dst).collect(),
                                    atoms,
                                }),
                                origin: Some(origin.ok_or(Refused::Source)?),
                                next: Some(next),
                            },
                        },
                        None => RegionOp::Boundary {
                            spec: BoundarySpec {
                                at: at.clone(),
                                action: None,
                                origin,
                                next: None,
                            },
                        },
                    }
                }
                (
                    doc::Node::Expr(doc::Expr::Act(act)),
                    Phase::Commit,
                    Some(next),
                    Some(dst),
                    Ok(_),
                ) if !matches!(act.kind, doc::ActKind::Call(_)) => RegionOp::Move {
                    dst,
                    src: sink,
                    next,
                },
                (doc::Node::Action(_), Phase::Commit, Some(next), Some(dst), Ok(_)) => {
                    RegionOp::Move {
                        dst,
                        src: sink,
                        next,
                    }
                }
                (
                    doc::Node::Expr(doc::Expr::Act(act)),
                    Phase::Enter,
                    Some(next),
                    None,
                    Ok(args),
                ) if matches!(act.kind, doc::ActKind::Call(_)) => {
                    let origin = origin.ok_or(Refused::Source)?;
                    let site = CallId(u32::try_from(calls.len()).map_err(|_| Refused::Facts)?);
                    calls.push(CallSite {
                        at: at.clone(),
                        origin,
                        atoms: Box::new([]),
                        arguments: if matches!(act.kind, doc::ActKind::Call(doc::Callee::Value(_)))
                        {
                            args.get(1..).ok_or(Refused::Facts)?.into()
                        } else {
                            args
                        },
                        result: sink,
                    });
                    RegionOp::Call { site, next }
                }
                (
                    doc::Node::Expr(doc::Expr::Variable(name)),
                    Phase::Commit,
                    Some(next),
                    Some(dst),
                    _,
                ) => {
                    if let Some(slot) = resources::role(
                        &source,
                        &Owner::Visible {
                            at: at.site.clone(),
                            name: name.clone(),
                        },
                    ) {
                        RegionOp::Checked {
                            kernel: CheckedKernel {
                                kind: CheckedKind::Local { slot },
                            },
                            args: Box::new([]),
                            dst,
                            next,
                        }
                    } else {
                        RegionOp::Boundary {
                            spec: BoundarySpec {
                                action: None,
                                at: at.clone(),
                                origin,
                                next: Some(next),
                            },
                        }
                    }
                }
                (
                    doc::Node::Expr(doc::Expr::Member(member)),
                    Phase::Commit,
                    Some(next),
                    Some(dst),
                    Ok(args),
                ) if !args.is_empty() => {
                    let selector = match &**member {
                        doc::Member::Field { field, .. } => {
                            Selector::Field(Arc::from(field.as_str()))
                        }
                        doc::Member::Index { .. } => {
                            Selector::Index(*args.get(1).ok_or(Refused::Facts)?)
                        }
                    };
                    let retained = resources::retained_read(&source, &at.site.child(0));
                    RegionOp::Read {
                        spec: ReadSpec {
                            at: at.clone(),
                            selector,
                            retained,
                            kind: RegisterKind::Value,
                        },
                        base: args[0],
                        dst,
                        next,
                    }
                }
                (
                    doc::Node::Expr(doc::Expr::Call { function, .. }),
                    Phase::Commit,
                    Some(next),
                    Some(dst),
                    Ok(args),
                ) => {
                    let lib = exe.library_lib(function).ok_or(Refused::Source)?;
                    depth = exe
                        .lib(lib)
                        .prepared_depth(exe)
                        .checked_add(u32::from(exe.lib(lib).operation()))
                        .ok_or(Refused::Facts)?;
                    if matches!(source.calls.get(function), Some(CallSource::Body(_))) {
                        if let Some(origin) = origin {
                            let site =
                                CallId(u32::try_from(calls.len()).map_err(|_| Refused::Facts)?);
                            calls.push(CallSite {
                                at: at.clone(),
                                origin,
                                atoms: Box::new([]),
                                arguments: args,
                                result: dst,
                            });
                            RegionOp::Call { site, next }
                        } else {
                            RegionOp::Boundary {
                                spec: BoundarySpec {
                                    action: None,
                                    at: at.clone(),
                                    origin,
                                    next: Some(next),
                                },
                            }
                        }
                    } else {
                        RegionOp::Checked {
                            kernel: CheckedKernel {
                                kind: CheckedKind::Native {
                                    function: *function,
                                    lib,
                                },
                            },
                            args,
                            dst,
                            next,
                        }
                    }
                }
                (_, Phase::Commit | Phase::Test, Some(next), Some(dst), Ok(args))
                    if args.len() == 1
                        && event.entities.is_empty()
                        && match node {
                            doc::Node::Stmt(doc::Stmt::Let { .. })
                            | doc::Node::Expr(doc::Expr::If { .. }) => at.phase == Phase::Commit,
                            doc::Node::Expr(doc::Expr::Let { .. }) => true,
                            _ => false,
                        } =>
                {
                    RegionOp::Move {
                        dst,
                        src: args[0],
                        next,
                    }
                }
                (
                    doc::Node::Stmt(doc::Stmt::Assign {
                        place: doc::Place::Variable(name),
                        ..
                    }),
                    Phase::Commit,
                    Some(next),
                    _,
                    Ok(args),
                ) if args.len() == 1
                    && event.entities.is_empty()
                    && !source.assigns_cell(&at.site) =>
                {
                    if let Ok(dst) = reg(
                        &roles,
                        &source,
                        &Owner::Visible {
                            at: at.site.clone(),
                            name: name.clone(),
                        },
                    ) {
                        RegionOp::Move {
                            dst,
                            src: args[0],
                            next,
                        }
                    } else {
                        RegionOp::Boundary {
                            spec: BoundarySpec {
                                action: None,
                                at: at.clone(),
                                origin,
                                next: Some(next),
                            },
                        }
                    }
                }
                (
                    doc::Node::Stmt(
                        doc::Stmt::Print { .. } | doc::Stmt::Finish { .. } | doc::Stmt::Fail { .. },
                    ),
                    Phase::Commit,
                    next,
                    None,
                    Ok(args),
                ) if args.len() == 1 && event.entities.is_empty() => RegionOp::Effect {
                    kind: match node {
                        doc::Node::Stmt(doc::Stmt::Print { .. }) => EffectKind::Print,
                        doc::Node::Stmt(doc::Stmt::Finish { .. }) => EffectKind::Finish,
                        _ => EffectKind::Fail,
                    },
                    value: args[0],
                    // Finishing or failing halts the program.
                    next: next.filter(|_| matches!(node, doc::Node::Stmt(doc::Stmt::Print { .. }))),
                },
                (doc::Node::Block(_), Phase::Leave, None, _, _) if at.site == source.site => {
                    RegionOp::Exit {
                        prefix: PrefixId(
                            u32::try_from(steps.len() * 3 + 2).map_err(|_| Refused::Facts)?,
                        ),
                    }
                }
                (doc::Node::Block(_), Phase::Enter | Phase::Leave, Some(next), _, _)
                    if event.entities.is_empty() =>
                {
                    scalar_resources = true;
                    RegionOp::Checked {
                        kernel: CheckedKernel {
                            kind: CheckedKind::Phase,
                        },
                        args: Box::new([]),
                        dst: sink,
                        next,
                    }
                }
                (_, Phase::Commit, Some(next), None, Ok(args))
                    if event.entities.is_empty()
                        && matches!(
                            node,
                            doc::Node::Stmt(
                                doc::Stmt::Do { .. }
                                    | doc::Stmt::If { .. }
                                    | doc::Stmt::While { .. }
                            )
                        ) =>
                {
                    RegionOp::Checked {
                        kernel: CheckedKernel {
                            kind: CheckedKind::Phase,
                        },
                        args,
                        dst: sink,
                        next,
                    }
                }
                (_, _, next, dst, Ok(args))
                    if heap::source_kind(exe, code, &source, at)?.is_some() =>
                {
                    RegionOp::Heap {
                        kernel: HeapKernel::compile(
                            &source,
                            &roles,
                            &event,
                            steps.len(),
                            heap::source_kind(exe, code, &source, at)?.ok_or(Refused::Source)?,
                        )?,
                        args,
                        dst,
                        next: next.unwrap_or(pc(at)?),
                    }
                }
                _ if !event.entities.is_empty()
                    || matches!(
                        node,
                        doc::Node::Expr(
                            doc::Expr::List(_)
                                | doc::Expr::Tuple(_)
                                | doc::Expr::Map(_)
                                | doc::Expr::Set(_)
                                | doc::Expr::Record(_)
                                | doc::Expr::Closure(_)
                        ) | doc::Node::Stmt(doc::Stmt::Assign { .. } | doc::Stmt::Remove { .. })
                    ) =>
                {
                    return Err(Refused::Source);
                }
                _ => RegionOp::Boundary {
                    spec: BoundarySpec {
                        action: None,
                        at: at.clone(),
                        origin,
                        next,
                    },
                },
            },
        };
        let before = PrefixId(
            u32::try_from(steps.len().checked_mul(3).ok_or(Refused::Facts)?)
                .map_err(|_| Refused::Facts)?,
        );
        steps.push(Step {
            at: at.clone(),
            peak: IndexedAmount::compile(&source, &roles, &event.peak, event.output.as_ref())?,
            delta: IndexedAmount::compile(&source, &roles, &event.delta, event.output.as_ref())?,
            event,
            before,
            pending: PrefixId(before.0.checked_add(1).ok_or(Refused::Facts)?),
            after: PrefixId(before.0.checked_add(2).ok_or(Refused::Facts)?),
            charges: closes.get(at).cloned().unwrap_or_default().into(),
            depth,
            scalar_resources,
            reachable: false,
            stage: None,
            scratch: source
                .room
                .scratch
                .iter()
                .filter(|scratch| scratch.at == *at)
                .map(|scratch| IndexedAmount::compile(&source, &roles, &scratch.amount, None))
                .collect::<Result<_, _>>()?,
            guard: None,
        });
        ops.push(op);
    }
    let mut draft = RegionDraft {
        executable: exe.local(),
        code,
        source,
        entry: pc(&entry)?,
        ops: ops.into(),
        generic_ops: Box::new([]),
        steps: steps.into(),
        generic_steps: Box::new([]),
        recovery: Box::new([]),
        roles,
        registers: registers.into(),
        generic_registers: Box::new([]),
        packing: Box::new([]),
        call_sources: calls
            .iter()
            .map(|call| (call.origin.statement, call.origin.pc))
            .collect(),
        calls: calls.into(),
        loops: program.loops,
        departures: Box::new([]),
    };
    super::stages::append(&mut draft, sink)?;
    super::departures::append(&mut draft, sink)?;
    super::joins::append(&mut draft)?;
    draft.generic_ops = draft.ops.clone();
    draft.generic_steps = draft.steps.clone();
    super::specialize::specialize(exe, &mut draft)?;
    draft.generic_registers = draft
        .registers
        .iter()
        .enumerate()
        .map(|(index, kind)| {
            if index > draft.roles.len() && *kind == RegisterKind::Bool {
                RegisterKind::Bool
            } else {
                RegisterKind::Value
            }
        })
        .collect();
    draft.packing = draft
        .registers
        .iter()
        .enumerate()
        .map(|(index, kind)| {
            Ok(match kind {
                RegisterKind::Int => Some(CheckedKernel {
                    kind: CheckedKind::PackInt {
                        input: Reg::new(index)?,
                    },
                }),
                RegisterKind::Float => Some(CheckedKernel {
                    kind: CheckedKind::PackFloat {
                        input: Reg::new(index)?,
                    },
                }),
                RegisterKind::Bool => Some(CheckedKernel {
                    kind: CheckedKind::PackBool {
                        input: Reg::new(index)?,
                    },
                }),
                RegisterKind::Value => None,
            })
        })
        .collect::<Result<_, Refused>>()?;
    let states = recovery::liveness(&draft)?;
    for ((step, generic), state) in draft
        .steps
        .iter_mut()
        .zip(draft.generic_steps.iter_mut())
        .zip(&states)
    {
        step.reachable = state.is_some();
        generic.reachable = state.is_some();
        generic.scalar_resources = generic.event.entities.is_empty()
            && match &draft.generic_ops[step.before.offset() / 3] {
                RegionOp::Constant { .. } | RegionOp::Branch { .. } => true,
                RegionOp::Read { spec, .. } => spec.retained.is_some(),
                RegionOp::Checked { kernel, .. } => matches!(
                    kernel.kind(),
                    CheckedKind::Local { .. } | CheckedKind::LoopControl { .. }
                ),
                _ => false,
            };
    }
    let mut maps = Vec::new();
    for (index, state) in states.iter().enumerate() {
        let before = state.clone().unwrap_or_default();
        let after = recovery::transfer(&draft, index, &before)?;
        let incoming_before = draft.steps[index]
            .event
            .inputs
            .iter()
            .any(|owner| matches!(owner, Owner::Incoming(_)))
            .then_some(sink);
        let incoming_after = delivers(&draft.ops[index]).then_some(sink);
        let incoming_pending = incoming_after.or(incoming_before);
        let mut pending_live = if incoming_after.is_some() {
            after.clone()
        } else {
            before.clone()
        };
        let pending = match &draft.ops[index] {
            op if delivers(op) => Some(sink),
            RegionOp::Heap { kernel, .. } if kernel.atomic_move() => None,
            RegionOp::Heap { .. } => draft.steps[index]
                .event
                .output
                .as_ref()
                .map(|owner| {
                    let reg = reg(&draft.roles, &draft.source, owner)?;
                    if let Role::Slot(slot) =
                        resources::role(&draft.source, owner).ok_or(Refused::Facts)?
                    {
                        pending_live.insert(slot);
                    }
                    Ok::<_, Refused>(reg)
                })
                .transpose()?,
            _ => draft.steps[index].event.output.as_ref().map(|_| sink),
        };
        for (phase, (live, pending, incoming)) in [
            (&before, incoming_before, incoming_before),
            (&pending_live, pending, incoming_pending),
            (&after, incoming_after, incoming_after),
        ]
        .into_iter()
        .enumerate()
        {
            let roles: Box<[_]> = draft
                .roles
                .iter()
                .filter_map(|(role, reg)| {
                    role.slot()
                        .filter(|slot| live.contains(slot))
                        .map(|slot| (slot, *reg))
                })
                .collect();
            let operands = roles
                .iter()
                .filter(|(slot, _)| {
                    matches!(
                        slot,
                        Slot::Operand(_) | Slot::Continuation(_) | Slot::Argument(_)
                    )
                })
                .map(|(_, reg)| *reg)
                .collect();
            maps.push(RecoveryMap {
                graph: GraphRecovery::from_roles(
                    &draft.steps[index].at,
                    &roles,
                    pending,
                    phase as u32,
                    incoming,
                ),
                roles,
                operands,
                pending,
                continuation: draft.steps[index].at.clone(),
            });
        }
    }
    draft.recovery = maps.into();
    Ok(draft)
}

/// Whether the step's completion is delivered into the sink as the task's
/// Incoming: a call's return.
pub(super) fn delivers(op: &RegionOp) -> bool {
    matches!(op, RegionOp::Call { .. })
}

/// A statement action's atoms, each loaded into its own slot from a visible
/// name's role or a constant; `None` when one has no such source.
pub(super) fn action_atoms(
    roles: &[(Role, Reg<LeaseValue>)],
    source: &Source,
    site: &Site,
    action: &doc::Action,
) -> Option<Box<[AtomLoad]>> {
    crate::compile::room::action_atoms(action)
        .iter()
        .enumerate()
        .map(|(index, atom)| {
            let dst = reg(
                roles,
                source,
                &Owner::Atom {
                    at: site.clone(),
                    index: u32::try_from(index).ok()?,
                },
            )
            .ok()?;
            let src = match atom {
                doc::Atom::Variable(name) => AtomSource::Role(
                    reg(
                        roles,
                        source,
                        &Owner::Visible {
                            at: site.clone(),
                            name: name.clone(),
                        },
                    )
                    .ok()?,
                ),
                doc::Atom::Literal(value) => AtomSource::Constant(literal(value)?),
            };
            Some(AtomLoad { dst, src })
        })
        .collect()
}
