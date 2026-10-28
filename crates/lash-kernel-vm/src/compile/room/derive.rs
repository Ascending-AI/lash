//! Formation reads the unchanged source tree before layout placement.
use super::*;

pub(super) fn body(authority: &Description) -> SourceRoom {
    let source = &authority.site;
    let body = &authority.body;
    let params = &authority.params;
    let cells = &authority.shared;
    let calls = &authority.calls;
    let mut room = SourceRoom {
        source: source.clone(),
        bulk: authority.bulk.and_then(|path| match authority.owner {
            doc::constant_pool::PoolOwner::Function(function) => {
                Some(bulk::Bulk::project(function, path))
            }
            _ => None,
        }),
        peak_extra: BTreeMap::new(),
        retained_delta: BTreeMap::new(),
        scratch: Box::new([]),
        exits: Box::new([]),
    };
    let mut scratch = Vec::new();
    let mut exits = Vec::new();
    let mut pending = vec![(source.clone(), doc::Node::Block(body))];
    while let Some((site, node)) = pending.pop() {
        let children = node.children();
        for (i, child_node) in children.into_iter().enumerate().rev() {
            pending.push((child(&site, i), child_node));
        }
        let ins = inputs(&site, node);
        let out = match node {
            doc::Node::Expr(_) | doc::Node::Action(_) => Some(operand(&site)),
            doc::Node::Stmt(doc::Stmt::Let { name, .. }) => {
                Some(if authority.persistent.contains_key(&site) {
                    Owner::SessionCell {
                        at: site.clone(),
                        name: name.clone(),
                    }
                } else {
                    Owner::Binding {
                        declared: site.clone(),
                        name: name.clone(),
                    }
                })
            }
            doc::Node::Stmt(
                doc::Stmt::Return { .. }
                | doc::Stmt::Throw { .. }
                | doc::Stmt::Finish { .. }
                | doc::Stmt::Fail { .. },
            ) => Some(Owner::Departure(site.clone())),
            _ => None,
        };
        let mut events = Vec::new();
        if let doc::Node::Block(_) = node {
            let frame = if site == *source {
                Cost::FRAME + params.iter().map(|n| shared(cells, &site, n)).sum::<u64>()
            } else {
                0
            };
            events.push(event(
                &site,
                Phase::Enter,
                Box::new([]),
                None,
                Amount::fixed(frame),
                Amount::fixed(frame),
            ));
            if site == *source {
                let n = Amount::fixed(Cost::REFERENCE);
                events.push(event(
                    &site,
                    Phase::Fallthrough,
                    Box::new([]),
                    Some(Owner::Departure(site.clone())),
                    n.clone(),
                    n,
                ));
            }
            let mut delta = Amount::default().term(
                release([Owner::Scope(site.clone()), Owner::Operands(site.clone())]),
                -1,
            );
            if site == *source {
                delta.fixed -= i128::from(Cost::FRAME);
            }
            events.push(event(
                &site,
                Phase::Leave,
                Box::new([]),
                None,
                Amount::default(),
                delta,
            ));
        } else {
            match node {
                doc::Node::Action(action) => {
                    let n = atoms(action).iter().enumerate().fold(
                        Amount::default(),
                        |amount, (index, _)| {
                            amount.term(
                                Measure::Local(Owner::Atom {
                                    at: site.clone(),
                                    index: index as u32,
                                }),
                                1,
                            )
                        },
                    );
                    events.push(event(&site, Phase::Enter, Box::new([]), None, n.clone(), n));
                }
                doc::Node::Expr(doc::Expr::Act(_)) => events.push(event(
                    &site,
                    Phase::Enter,
                    ins.clone(),
                    None,
                    Amount::default(),
                    Amount::default(),
                )),
                doc::Node::Stmt(doc::Stmt::Try(doc::TryStmt {
                    catch: Some(catch), ..
                })) => {
                    let n = Amount::fixed(shared(cells, &child(&site, 1), &catch.binding));
                    events.push(event(
                        &site,
                        Phase::Catch,
                        vec![Owner::ActiveDeparture(source.clone())].into(),
                        Some(Owner::Binding {
                            declared: child(&site, 1),
                            name: catch.binding.clone(),
                        }),
                        n.clone(),
                        n,
                    ));
                }
                doc::Node::Expr(doc::Expr::If { .. })
                | doc::Node::Stmt(doc::Stmt::If { .. } | doc::Stmt::While { .. }) => {
                    let cond = operand(&child(&site, 0));
                    events.push(event(
                        &site,
                        Phase::Test,
                        vec![cond.clone()].into(),
                        None,
                        Amount::default(),
                        Amount::default().term(release([cond]), -1),
                    ));
                }
                doc::Node::Expr(doc::Expr::Let { name, .. }) => {
                    let n = shared(cells, &site, name);
                    events.push(event(
                        &site,
                        Phase::Test,
                        vec![operand(&child(&site, 0))].into(),
                        Some(Owner::Binding {
                            declared: site.clone(),
                            name: name.clone(),
                        }),
                        Amount::fixed(n),
                        Amount::fixed(n),
                    ));
                }
                doc::Node::Stmt(doc::Stmt::For { binding, via, .. }) => {
                    if let Some(via) = via {
                        let hooks = [
                            (Phase::Open, Some(&via.open), 1),
                            (Phase::Next, via.next.as_ref(), 1),
                            (Phase::Check, via.check.as_ref(), 4),
                            (Phase::Item, via.item.as_ref(), 2),
                        ];
                        for (phase, target, count) in hooks {
                            if let Some(target) = target {
                                let args: Box<[_]> = (0..count)
                                    .map(|index| Owner::HookArgument {
                                        at: site.clone(),
                                        phase,
                                        index,
                                    })
                                    .collect();
                                let callee = hook(target, calls);
                                let base = args
                                    .iter()
                                    .cloned()
                                    .fold(Amount::default(), |a, o| a.term(Measure::Local(o), 1));
                                let peak = base.clone().term(
                                    Measure::CallPeak {
                                        at: site.clone(),
                                        callee,
                                    },
                                    1,
                                );
                                let mut delta = base.term(
                                    Measure::CallDelta {
                                        at: site.clone(),
                                        callee,
                                    },
                                    1,
                                );
                                if matches!(callee, CallSource::Native(_) | CallSource::Dynamic) {
                                    delta = delta.term(
                                        Measure::NativeRelease {
                                            at: site.clone(),
                                            callee,
                                            owners: args.clone(),
                                        },
                                        -1,
                                    );
                                }
                                delta = delta.term(
                                    release([Owner::HookResult {
                                        at: site.clone(),
                                        phase,
                                    }]),
                                    -1,
                                );
                                events.push(event(
                                    &site,
                                    phase,
                                    args,
                                    Some(Owner::HookResult {
                                        at: site.clone(),
                                        phase,
                                    }),
                                    peak,
                                    delta,
                                ));
                            }
                        }
                    }
                    let at = child(&site, 1);
                    let binding_owner = Owner::Binding {
                        declared: at.clone(),
                        name: binding.clone(),
                    };
                    let mut delta = Amount::fixed(shared(cells, &at, binding));
                    if via.as_ref().is_none_or(|v| v.item.is_none()) {
                        delta = delta.term(Measure::Local(binding_owner.clone()), 1);
                    }
                    events.push(event(
                        &site,
                        Phase::Test,
                        Box::new([]),
                        Some(binding_owner),
                        delta.clone(),
                        delta,
                    ));
                }
                _ => {}
            }
            if let doc::Node::Stmt(doc::Stmt::Try(t)) = node
                && t.finally.is_some()
            {
                let active = Owner::ActiveDeparture(source.clone());
                let pending = Owner::PendingDeparture(site.clone());
                events.push(event(
                    &site,
                    Phase::Suspend,
                    vec![active.clone()].into(),
                    Some(pending.clone()),
                    Amount::default(),
                    Amount::default(),
                ));
                events.push(event(
                    &site,
                    Phase::Restore,
                    vec![pending.clone()].into(),
                    Some(active),
                    Amount::default(),
                    Amount::default(),
                ));
                events.push(event(
                    &site,
                    Phase::Supersede,
                    vec![pending.clone()].into(),
                    None,
                    Amount::default(),
                    Amount::default().term(release([pending]), -1),
                ));
            }
            let (peak, delta) = formula(authority, &site, node, &ins);
            events.push(event(
                &site,
                Phase::Commit,
                ins.clone(),
                out.clone(),
                peak,
                delta,
            ));
        }
        for mut e in events {
            e.entities = entities(node, &site, e.point.phase, source, params, cells);
            let kind = exit(node, e.point.phase);
            let owners: Box<[_]> = vec![
                Owner::Frame(source.clone()),
                Owner::Scope(source.clone()),
                Owner::Operands(source.clone()),
                Owner::Departures(source.clone()),
            ]
            .into_iter()
            .chain((source.unit == Unit::Main).then(|| Owner::Sessions(source.clone())))
            .collect();
            exits.push(Exit {
                point: e.point.clone(),
                charge_prefix: e.point.clone(),
                kind,
                owners: owners.clone(),
            });
            if (matches!(node, doc::Node::Expr(_) | doc::Node::Action(_))
                || matches!(
                    e.point.phase,
                    Phase::Open | Phase::Next | Phase::Check | Phase::Item
                ))
                && kind != ExitKind::Raise
            {
                exits.push(Exit {
                    point: e.point.clone(),
                    charge_prefix: e.point.clone(),
                    kind: ExitKind::Raise,
                    owners: owners.clone(),
                });
            }
            for measure in e.peak.terms.keys() {
                if let Measure::CallPeak {
                    callee: CallSource::Native(function),
                    ..
                } = measure
                {
                    scratch.push(Scratch {
                        at: e.point.clone(),
                        function: *function,
                        amount: Amount::default().term(
                            Measure::NativeScratch {
                                at: site.clone(),
                                function: *function,
                            },
                            1,
                        ),
                        normal: e.point.clone(),
                        raised: e.point.clone(),
                    });
                }
            }
            room.peak_extra.insert(e.point.clone(), e.clone());
            room.retained_delta.insert(e.point.clone(), e);
        }
    }
    room.scratch = scratch.into();
    room.exits = exits.into();
    room
}
fn event(
    site: &Site,
    phase: Phase,
    inputs: Box<[Owner]>,
    output: Option<Owner>,
    peak: Amount,
    delta: Amount,
) -> Event {
    Event {
        point: point(site, phase),
        inputs,
        output,
        entities: Box::new([]),
        peak,
        delta,
    }
}

fn formula(
    authority: &Description,
    site: &Site,
    node: doc::Node<'_>,
    ins: &[Owner],
) -> (Amount, Amount) {
    let cells = &authority.shared;
    let calls = &authority.calls;
    let zero = Amount::default();
    let result = || Amount::default().term(Measure::Local(operand(site)), 1);
    let consumed = || release(ins.iter().cloned());
    match node {
        doc::Node::Expr(expr) => match expr {
            doc::Expr::Literal(_) | doc::Expr::Variable(_) => (result(), result()),
            doc::Expr::Tuple(_) => (
                Amount::fixed(Cost::REFERENCE),
                result().term(consumed(), -1),
            ),
            doc::Expr::List(_) | doc::Expr::Set(_) | doc::Expr::Map(_) | doc::Expr::Record(_) => {
                let n = Amount::fixed(Cost::OBJECT + Cost::REFERENCE)
                    .term(Measure::Members(site.clone()), 1);
                let delta = n.clone().term(consumed(), -1);
                (n, delta)
            }
            doc::Expr::Closure(_) => {
                let n = Amount::fixed(Cost::OBJECT + Cost::REFERENCE).term(
                    Measure::CaptureEdges(site.clone()),
                    i128::from(Cost::ENTITY_EDGE),
                );
                (n.clone(), n)
            }
            doc::Expr::Member(_) => (result(), result().term(consumed(), -1)),
            doc::Expr::Call { function, .. } => {
                let callee = calls[function];
                let peak = Amount::default().term(
                    Measure::CallPeak {
                        at: site.clone(),
                        callee,
                    },
                    1,
                );
                let mut delta = Amount::default().term(
                    Measure::CallDelta {
                        at: site.clone(),
                        callee,
                    },
                    1,
                );
                if matches!(callee, CallSource::Native(_)) {
                    delta = delta.term(consumed(), -1);
                }
                (peak, delta)
            }
            doc::Expr::If { .. } => (zero.clone(), zero),
            doc::Expr::Let { name, .. } => (
                zero.clone(),
                zero.term(
                    release([Owner::Binding {
                        declared: site.clone(),
                        name: name.clone(),
                    }]),
                    -1,
                ),
            ),
            doc::Expr::Act(_) => (result(), result().term(consumed(), -1)),
        },
        doc::Node::Stmt(stmt) => match stmt {
            doc::Stmt::Let { name, .. } => {
                let n = Amount::fixed(shared(cells, site, name));
                let delta = if authority.persistent.contains_key(site) {
                    n.clone().term(
                        release([Owner::SessionCell {
                            at: site.clone(),
                            name: name.clone(),
                        }]),
                        -1,
                    )
                } else {
                    n.clone()
                };
                (n, delta)
            }
            doc::Stmt::Assign {
                place: doc::Place::Variable(name),
                ..
            } => (
                zero.clone(),
                zero.term(
                    release([Owner::Visible {
                        at: site.clone(),
                        name: name.clone(),
                    }]),
                    -1,
                ),
            ),
            doc::Stmt::Assign { .. } | doc::Stmt::Remove { .. } => {
                let peak = ins
                    .iter()
                    .cloned()
                    .fold(zero.clone(), |a, o| a.term(Measure::Local(o), 1));
                (
                    peak.term(Measure::MemberGrowth(site.clone()), 1),
                    zero.term(Measure::MemberDelta(site.clone()), 1)
                        .term(consumed(), -1),
                )
            }
            doc::Stmt::Do { .. } | doc::Stmt::Print { .. } => {
                (zero.clone(), zero.term(consumed(), -1))
            }
            doc::Stmt::For { .. } => (
                zero.clone(),
                zero.term(release([Owner::Operands(site.clone())]), -1),
            ),
            _ => (zero.clone(), zero),
        },
        doc::Node::Action(action) => (
            result(),
            result().term(
                release(atoms(action).iter().enumerate().map(|(i, _)| Owner::Atom {
                    at: site.clone(),
                    index: i as u32,
                })),
                -1,
            ),
        ),
        doc::Node::Block(_) => unreachable!("block has entry and release events"),
    }
}
fn exit(node: doc::Node<'_>, phase: Phase) -> ExitKind {
    if phase == Phase::Enter
        && matches!(
            node,
            doc::Node::Action(_) | doc::Node::Expr(doc::Expr::Act(_))
        )
    {
        return ExitKind::Action;
    }
    if phase != Phase::Commit {
        return ExitKind::Normal;
    }
    match node {
        doc::Node::Stmt(doc::Stmt::Return { .. }) => ExitKind::Return,
        doc::Node::Stmt(doc::Stmt::ReturnVia { .. }) => ExitKind::Action,
        doc::Node::Stmt(doc::Stmt::Throw { .. }) => ExitKind::Raise,
        doc::Node::Stmt(doc::Stmt::Finish { .. }) => ExitKind::Finish,
        doc::Node::Stmt(doc::Stmt::Fail { .. }) => ExitKind::Fail,
        doc::Node::Stmt(doc::Stmt::Break) => ExitKind::Break,
        doc::Node::Stmt(doc::Stmt::Continue) => ExitKind::Continue,
        _ => ExitKind::Normal,
    }
}

fn entities(
    node: doc::Node<'_>,
    site: &Site,
    phase: Phase,
    source: &Site,
    params: &[Name],
    cells: &BTreeMap<(Site, Name), bool>,
) -> Box<[Owner]> {
    let cell = |declared: &Site, name: &Name| {
        (shared(cells, declared, name) > 0).then(|| Owner::Cell {
            declared: declared.clone(),
            name: name.clone(),
        })
    };
    match (node, phase) {
        (doc::Node::Block(_), Phase::Enter) if site == source => {
            params.iter().filter_map(|name| cell(site, name)).collect()
        }
        (
            doc::Node::Expr(
                doc::Expr::List(_)
                | doc::Expr::Map(_)
                | doc::Expr::Set(_)
                | doc::Expr::Record(_)
                | doc::Expr::Closure(_),
            ),
            Phase::Commit,
        ) => vec![Owner::Entity(site.clone())].into(),
        (doc::Node::Stmt(doc::Stmt::Let { name, .. }), Phase::Commit)
        | (doc::Node::Expr(doc::Expr::Let { name, .. }), Phase::Test) => {
            cell(site, name).into_iter().collect()
        }
        (doc::Node::Stmt(doc::Stmt::For { binding, .. }), Phase::Test) => {
            cell(&child(site, 1), binding).into_iter().collect()
        }
        (doc::Node::Stmt(doc::Stmt::Try(t)), Phase::Catch) => t
            .catch
            .iter()
            .filter_map(|c| cell(&child(site, 1), &c.binding))
            .collect(),
        _ => Box::new([]),
    }
}
