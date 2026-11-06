//! Independent source oracle. This walks the AST and states each required
//! coefficient directly; it never invokes formation or reads a plan's totals.
use super::*;

pub(super) fn body(source: &Source, room: &SourceRoom) -> Result<(), Refused> {
    if room.source != source.site {
        return Err(Refused::Source);
    }
    if let doc::constant_pool::PoolOwner::Function(function) = source.owner
        && !matches!(source.site.unit,Unit::Library(f) if f==function)
    {
        return Err(Refused::Source);
    }
    bulk::verify(source, room)?;
    let mut exit_at = BTreeMap::new();
    for e in &room.exits {
        if exit_at.insert((e.point.clone(), e.kind), e).is_some() {
            return Err(Refused::Exit);
        }
    }
    let mut points = std::collections::BTreeSet::new();
    let mut expected_scratch = Vec::new();
    let mut exit_count = 0;
    let mut pending = vec![(source.site.clone(), doc::Node::Block(&source.body))];
    while let Some((site, node)) = pending.pop() {
        for (i, c) in node.children().into_iter().enumerate() {
            pending.push((child(&site, i), c));
        }
        let mut phases = vec![Phase::Commit];
        match node {
            doc::Node::Block(_) => {
                phases = vec![Phase::Enter, Phase::Leave];
                if site == source.site {
                    phases.push(Phase::Fallthrough);
                }
            }
            doc::Node::Action(_) => phases.push(Phase::Enter),
            doc::Node::Expr(doc::Expr::Act(act)) => {
                if matches!(
                    act.kind,
                    doc::ActKind::Call(doc::Callee::Value(_))
                        | doc::ActKind::Spawn(doc::Callee::Value(_))
                ) {
                    phases.push(Phase::Open);
                }
                phases.push(Phase::Enter);
                if super::act_calls(act) && native_act(node, &source.calls).is_none() {
                    phases.push(Phase::Restore);
                }
            }
            doc::Node::Expr(doc::Expr::Call { function, .. })
                if matches!(source.calls.get(function), Some(CallSource::Body(_))) =>
            {
                phases.push(Phase::Restore);
            }
            doc::Node::Stmt(doc::Stmt::Try(t)) => {
                if t.catch.is_some() {
                    phases.push(Phase::Catch);
                }
                if t.finally.is_some() {
                    phases.extend([Phase::Suspend, Phase::Restore, Phase::Supersede]);
                }
            }
            doc::Node::Expr(doc::Expr::If { .. } | doc::Expr::Let { .. })
            | doc::Node::Stmt(doc::Stmt::If { .. }) => phases.push(Phase::Test),
            doc::Node::Stmt(doc::Stmt::While { .. } | doc::Stmt::For { .. }) => {
                phases.extend([Phase::Test, Phase::Catch]);
            }
            _ => {}
        }
        if let doc::Node::Stmt(doc::Stmt::For { via: None, .. }) = node {
            phases.push(Phase::Enter);
        }
        if let doc::Node::Stmt(doc::Stmt::For { via: Some(via), .. }) = node {
            phases.push(Phase::Open);
            if via.next.is_some() {
                phases.push(Phase::Next);
            }
            if via.item.is_some() {
                phases.push(Phase::Item);
            }
            if via.check.is_some() {
                phases.push(Phase::Check);
            }
        }
        for phase in phases {
            let at = point(&site, phase);
            points.insert(at.clone());
            let peak = room.events.get(&at).ok_or(Refused::Peak)?;
            let (fixed_peak, terms_peak, fixed_delta, terms_delta) =
                obligations(source, &site, node, phase);
            check(&peak.peak, fixed_peak, terms_peak, Refused::Peak)?;
            check(&peak.delta, fixed_delta, terms_delta, Refused::Retained)?;
            if peak.point != at {
                return Err(Refused::Source);
            }
            let mut expected_inputs = if phase == Phase::Commit {
                commit_inputs(&site, node)
            } else {
                inputs(&site, node)
            };
            let mut output = match node {
                doc::Node::Expr(_) | doc::Node::Action(_) => Some(operand(&site)),
                doc::Node::Stmt(doc::Stmt::Let { name, .. }) => {
                    Some(if source.persistent.contains_key(&site) {
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
                    | doc::Stmt::Fail { .. }
                    | doc::Stmt::Break
                    | doc::Stmt::Continue,
                ) => Some(Owner::ActiveDeparture(source.site.clone())),
                _ => None,
            };
            match (node, phase) {
                (doc::Node::Block(_), _) => {
                    expected_inputs = Box::new([]);
                    output = if phase == Phase::Fallthrough {
                        Some(Owner::ActiveDeparture(site.clone()))
                    } else {
                        None
                    };
                }
                (doc::Node::Action(_), Phase::Enter) => {
                    expected_inputs = Box::new([]);
                    output = None;
                }
                (doc::Node::Expr(doc::Expr::Act(_)), Phase::Open) => {
                    expected_inputs = Box::new([]);
                }
                (doc::Node::Expr(doc::Expr::Act(_)), Phase::Enter) => {
                    if delivers(node) {
                        expected_inputs = Box::new([]);
                    }
                    if native_act(node, &source.calls).is_none() {
                        output = None;
                    }
                }
                (doc::Node::Expr(doc::Expr::Act(_) | doc::Expr::Call { .. }), Phase::Restore) => {
                    expected_inputs = vec![Owner::Incoming(site.clone())].into();
                }
                (doc::Node::Stmt(doc::Stmt::Try(t)), Phase::Catch) => {
                    let Some(catch) = &t.catch else {
                        return Err(Refused::Source);
                    };
                    expected_inputs = vec![Owner::ActiveDeparture(source.site.clone())].into();
                    output = Some(Owner::Binding {
                        declared: child(&site, 1),
                        name: catch.binding.clone(),
                    });
                }
                (
                    doc::Node::Stmt(doc::Stmt::While { .. } | doc::Stmt::For { .. }),
                    Phase::Catch,
                ) => {
                    expected_inputs = vec![Owner::ActiveDeparture(source.site.clone())].into();
                    output = None;
                }
                (doc::Node::Stmt(doc::Stmt::Try(_)), Phase::Suspend) => {
                    expected_inputs = vec![Owner::ActiveDeparture(source.site.clone())].into();
                    output = Some(Owner::PendingDeparture(site.clone()));
                }
                (doc::Node::Stmt(doc::Stmt::Try(_)), Phase::Restore) => {
                    expected_inputs = vec![Owner::PendingDeparture(site.clone())].into();
                    output = Some(Owner::ActiveDeparture(source.site.clone()));
                }
                (doc::Node::Stmt(doc::Stmt::Try(_)), Phase::Supersede) => {
                    expected_inputs = vec![Owner::PendingDeparture(site.clone())].into();
                    output = None;
                }
                (doc::Node::Expr(doc::Expr::Let { name, .. }), Phase::Test) => {
                    expected_inputs = vec![operand(&child(&site, 0))].into();
                    output = Some(Owner::Binding {
                        declared: site.clone(),
                        name: name.clone(),
                    });
                }
                (doc::Node::Stmt(doc::Stmt::For { binding, via, .. }), Phase::Test) => {
                    expected_inputs = match via {
                        None => Box::new([]),
                        Some(via) if via.item.is_some() => Box::new([
                            Owner::Incoming(site.clone()),
                            Owner::LoopHeader(site.clone()),
                        ]),
                        Some(_) => Box::new([Owner::LoopHeader(site.clone())]),
                    };
                    output = Some(Owner::Binding {
                        declared: child(&site, 1),
                        name: binding.clone(),
                    });
                }
                (_, Phase::Test) => {
                    expected_inputs = vec![operand(&child(&site, 0))].into();
                    output = None;
                }
                (doc::Node::Stmt(doc::Stmt::For { via: None, .. }), Phase::Enter) => {
                    expected_inputs = Box::new([]);
                    output = None;
                }
                (
                    doc::Node::Stmt(doc::Stmt::For { .. }),
                    Phase::Open | Phase::Next | Phase::Item | Phase::Check,
                ) => {
                    expected_inputs = if phase == Phase::Open {
                        Box::new([operand(&child(&site, 0))])
                    } else {
                        Box::new([])
                    };
                    output = None;
                }
                _ => {}
            }
            if peak.point != at || peak.inputs != expected_inputs || peak.output != output {
                return Err(Refused::Source);
            }
            check_entities(source, peak, &site, node, phase)?;
            let kind = match (node, phase) {
                (doc::Node::Expr(doc::Expr::Act(_)) | doc::Node::Action(_), Phase::Enter)
                    if native_act(node, &source.calls).is_none() =>
                {
                    ExitKind::Action
                }
                (doc::Node::Stmt(doc::Stmt::ReturnVia { .. }), Phase::Commit) => ExitKind::Action,
                (doc::Node::Stmt(doc::Stmt::Return { .. }), Phase::Commit) => ExitKind::Return,
                (doc::Node::Stmt(doc::Stmt::Throw { .. }), Phase::Commit) => ExitKind::Raise,
                (doc::Node::Stmt(doc::Stmt::Finish { .. }), Phase::Commit) => ExitKind::Finish,
                (doc::Node::Stmt(doc::Stmt::Fail { .. }), Phase::Commit) => ExitKind::Fail,
                (doc::Node::Stmt(doc::Stmt::Break), Phase::Commit) => ExitKind::Break,
                (doc::Node::Stmt(doc::Stmt::Continue), Phase::Commit) => ExitKind::Continue,
                _ => ExitKind::Normal,
            };
            let kinds = if super::raises(node, phase) && kind != ExitKind::Raise {
                vec![kind, ExitKind::Raise]
            } else {
                vec![kind]
            };
            for kind in kinds {
                exit_count += 1;
                let e = exit_at.get(&(at.clone(), kind)).ok_or(Refused::Exit)?;
                if e.charge_prefix != at {
                    return Err(Refused::Exit);
                }
                let owners: Box<[_]> = vec![
                    Owner::Frame(source.site.clone()),
                    Owner::Scope(source.site.clone()),
                    Owner::Operands(source.site.clone()),
                    Owner::Departures(source.site.clone()),
                ]
                .into_iter()
                .chain(
                    (source.site.unit == Unit::Main).then(|| Owner::Sessions(source.site.clone())),
                )
                .collect();
                if e.owners != owners {
                    return Err(Refused::Exit);
                }
            }
        }
        if let doc::Node::Expr(doc::Expr::Call { function, .. }) = node {
            let callee = source.calls.get(function).ok_or(Refused::Source)?;
            if matches!(callee, CallSource::Native(_)) {
                expected_scratch.push((point(&site, Phase::Commit), *function));
            }
        }
        if let Some(function) = native_act(node, &source.calls) {
            expected_scratch.push((point(&site, Phase::Enter), function));
        }
        if let doc::Node::Stmt(doc::Stmt::For { via: Some(via), .. }) = node {
            for (phase, target) in [
                (Phase::Open, Some(&via.open)),
                (Phase::Next, via.next.as_ref()),
                (Phase::Check, via.check.as_ref()),
                (Phase::Item, via.item.as_ref()),
            ] {
                if let Some(doc::FunctionTarget::Local(function)) = target
                    && matches!(source.calls.get(function), Some(CallSource::Native(_)))
                {
                    expected_scratch.push((point(&site, phase), *function));
                }
            }
        }
    }
    if points.len() != room.events.len() {
        return Err(Refused::Source);
    }
    if exit_count != room.exits.len() {
        return Err(Refused::Exit);
    }
    if expected_scratch.len() != room.scratch.len() {
        return Err(Refused::Scratch);
    }
    for (site, function) in expected_scratch {
        let mut found = room
            .scratch
            .iter()
            .filter(|s| s.at == site && s.function == function);
        let s = found.next().ok_or(Refused::Scratch)?;
        let end = site.clone();
        check(
            &s.amount,
            0,
            vec![(
                Measure::NativeScratch {
                    at: site.site.clone(),
                    function,
                },
                1,
            )],
            Refused::Scratch,
        )?;
        if found.next().is_some() || s.normal != end || s.raised != end {
            return Err(Refused::Scratch);
        }
    }
    Ok(())
}
fn check(
    actual: &Amount,
    fixed: i128,
    mut terms: Vec<(Measure, i128)>,
    why: Refused,
) -> Result<(), Refused> {
    terms.sort_by(|(left, _), (right, _)| left.cmp(right));
    terms.dedup_by(|later, earlier| {
        if later.0 != earlier.0 {
            return false;
        }
        earlier.1 = later.1;
        true
    });
    if actual.fixed != fixed || actual.terms != terms {
        Err(why)
    } else {
        Ok(())
    }
}
type Terms = Vec<(Measure, i128)>;
fn obligations(
    source: &Source,
    site: &Site,
    node: doc::Node<'_>,
    phase: Phase,
) -> (i128, Terms, i128, Terms) {
    let mut p = 0;
    let mut d = 0;
    let mut peak = Vec::new();
    let mut delta = Vec::new();
    let ins = inputs(site, node);
    let consumed = || Measure::Release(ins.clone());
    match (node, phase) {
        (doc::Node::Block(_), Phase::Enter) if *site == source.site => {
            p = i128::from(Cost::FRAME);
            for name in &source.params {
                if source.shared.get(&(site.clone(), name.clone())) == Some(&true) {
                    p += i128::from(Cost::OBJECT) + i128::from(Cost::REFERENCE);
                }
            }
            d = p;
        }
        (doc::Node::Block(_), Phase::Fallthrough) => {
            p = Cost::REFERENCE.into();
            d = p;
        }
        (doc::Node::Block(_), Phase::Leave) => {
            if *site == source.site {
                d = -i128::from(Cost::FRAME);
            }
            delta.push((
                release([Owner::Scope(site.clone()), Owner::Operands(site.clone())]),
                -1,
            ));
        }
        (doc::Node::Expr(doc::Expr::Let { name, .. }), Phase::Test) => {
            if source.shared.get(&(site.clone(), name.clone())) == Some(&true) {
                p = i128::from(Cost::OBJECT) + i128::from(Cost::REFERENCE);
                d = p;
            }
        }
        (doc::Node::Stmt(doc::Stmt::For { binding, via, .. }), Phase::Test) => {
            let at = child(site, 1);
            if source.shared.get(&(at.clone(), binding.clone())) == Some(&true) {
                p = i128::from(Cost::OBJECT) + i128::from(Cost::REFERENCE);
                d = p;
            }
            match via {
                None => {
                    let local = Measure::Local(Owner::Binding {
                        declared: at,
                        name: binding.clone(),
                    });
                    peak.push((local.clone(), 1));
                    delta.push((local, 1));
                }
                Some(via) if via.item.is_some() => {
                    delta.push((release([Owner::LoopHeader(site.clone())]), -1));
                }
                Some(_) => {}
            }
        }
        (
            doc::Node::Stmt(doc::Stmt::For { via: Some(via), .. }),
            Phase::Open | Phase::Next | Phase::Item | Phase::Check,
        ) => {
            let target = hooks(via)
                .find_map(|(hook, target)| (hook == phase).then_some(target))
                .unwrap_or_else(|| unreachable!("source phases select only declared hooks"));
            let callee = match target {
                doc::FunctionTarget::Local(f) => source.calls[f],
                doc::FunctionTarget::Import(_) => CallSource::Dynamic,
            };
            peak.push((
                Measure::CallPeak {
                    at: site.clone(),
                    callee,
                },
                1,
            ));
            delta.push((
                Measure::CallDelta {
                    at: site.clone(),
                    callee,
                },
                1,
            ));
            if phase == Phase::Open && matches!(callee, CallSource::Native(_) | CallSource::Dynamic)
            {
                delta.push((release([operand(&child(site, 0))]), -1));
            }
        }
        (doc::Node::Stmt(doc::Stmt::Try(_)), Phase::Supersede) => {
            delta.push((release([Owner::PendingDeparture(site.clone())]), -1));
        }
        (doc::Node::Stmt(doc::Stmt::Try(_)), Phase::Suspend | Phase::Restore) => {}
        (doc::Node::Expr(doc::Expr::Act(act)), Phase::Open)
            if matches!(
                act.kind,
                doc::ActKind::Call(doc::Callee::Value(_))
                    | doc::ActKind::Spawn(doc::Callee::Value(_))
            ) =>
        {
            peak.push((Measure::Local(operand(site)), 1));
            delta.push((Measure::Local(operand(site)), 1));
        }
        (doc::Node::Expr(doc::Expr::Act(_) | doc::Expr::Call { .. }), Phase::Restore) => {}
        (doc::Node::Stmt(doc::Stmt::Try(t)), Phase::Catch) => {
            if let Some(c) = &t.catch
                && source.shared.get(&(child(site, 1), c.binding.clone())) == Some(&true)
            {
                p = i128::from(Cost::OBJECT) + i128::from(Cost::REFERENCE);
                d = p;
            }
        }
        (doc::Node::Expr(doc::Expr::Act(_)), Phase::Enter)
            if native_act(node, &source.calls).is_some() =>
        {
            let callee = source.calls[&native_act(node, &source.calls)
                .unwrap_or_else(|| unreachable!("the guard selected a native call act"))];
            peak.push((
                Measure::CallPeak {
                    at: site.clone(),
                    callee,
                },
                1,
            ));
            delta.push((
                Measure::CallDelta {
                    at: site.clone(),
                    callee,
                },
                1,
            ));
            delta.push((consumed(), -1));
        }
        (doc::Node::Action(action), Phase::Enter) => {
            for (index, _) in action_atoms(action).iter().enumerate() {
                peak.push((
                    Measure::Local(Owner::Atom {
                        at: site.clone(),
                        index: index as u32,
                    }),
                    1,
                ));
            }
            d = p;
            delta = peak.clone();
        }
        (_, Phase::Test) => delta.push((release([operand(&child(site, 0))]), -1)),
        (doc::Node::Expr(expr), Phase::Commit) => match expr {
            doc::Expr::Literal(doc::Literal::Function(_)) => {
                let value = Measure::Local(operand(site));
                peak.push((value.clone(), 1));
                delta.push((value, 1));
            }
            doc::Expr::Literal(_) => {
                let value = Measure::Local(operand(site));
                peak.push((value.clone(), 1));
                delta.push((value, 1));
            }
            doc::Expr::Act(act) if !super::act_calls(act) => {
                delta.push((consumed(), -1));
            }
            doc::Expr::Variable(_) | doc::Expr::Member(_) | doc::Expr::Act(_) => {
                let value = Measure::Local(operand(site));
                peak.push((value.clone(), 1));
                delta.push((value, 1));
                if matches!(expr, doc::Expr::Member(_) | doc::Expr::Act(_)) {
                    delta.push((consumed(), -1));
                }
            }
            doc::Expr::Tuple(_) => {
                p = Cost::REFERENCE.into();
                delta.push((Measure::Local(operand(site)), 1));
                delta.push((consumed(), -1));
            }
            doc::Expr::List(_) | doc::Expr::Map(_) | doc::Expr::Set(_) | doc::Expr::Record(_) => {
                p = i128::from(Cost::OBJECT) + i128::from(Cost::REFERENCE);
                d = p;
                peak.push((Measure::Members(site.clone()), 1));
                delta.push((Measure::Members(site.clone()), 1));
                delta.push((consumed(), -1));
            }
            doc::Expr::Closure(_) => {
                p = i128::from(Cost::OBJECT) + i128::from(Cost::REFERENCE);
                d = p;
                peak.push((
                    Measure::CaptureEdges(site.clone()),
                    Cost::ENTITY_EDGE.into(),
                ));
                delta = peak.clone();
            }
            doc::Expr::Call { function, .. } => {
                let callee = source.calls[function];
                peak.push((
                    Measure::CallPeak {
                        at: site.clone(),
                        callee,
                    },
                    1,
                ));
                delta.push((
                    Measure::CallDelta {
                        at: site.clone(),
                        callee,
                    },
                    1,
                ));
                if matches!(callee, CallSource::Native(_)) {
                    delta.push((consumed(), -1));
                }
            }
            doc::Expr::Let { name, .. } => delta.push((
                release([Owner::Binding {
                    declared: site.clone(),
                    name: name.clone(),
                }]),
                -1,
            )),
            doc::Expr::If { .. } => {}
        },
        (doc::Node::Stmt(stmt), Phase::Commit) => match stmt {
            doc::Stmt::Let { name, .. } => {
                if source.shared.get(&(site.clone(), name.clone())) == Some(&true) {
                    p = i128::from(Cost::OBJECT) + i128::from(Cost::REFERENCE);
                    d = p;
                }
                if source.persistent.contains_key(site) {
                    delta.push((
                        release([Owner::SessionCell {
                            at: site.clone(),
                            name: name.clone(),
                        }]),
                        -1,
                    ));
                }
            }
            doc::Stmt::Assign {
                place: doc::Place::Variable(name),
                ..
            } if !source.assigns_cell(site) => delta.push((
                release([Owner::Visible {
                    at: site.clone(),
                    name: name.clone(),
                }]),
                -1,
            )),
            doc::Stmt::Assign { .. } | doc::Stmt::Remove { .. } => {
                peak.extend(ins.iter().cloned().map(|o| (Measure::Local(o), 1)));
                peak.push((Measure::MemberGrowth(site.clone()), 1));
                delta.push((Measure::MemberDelta(site.clone()), 1));
                delta.push((consumed(), -1));
            }
            doc::Stmt::Do { .. } | doc::Stmt::Print { .. } => delta.push((consumed(), -1)),
            doc::Stmt::For { .. } => delta.push((release([Owner::Operands(site.clone())]), -1)),
            _ => {}
        },
        (doc::Node::Action(action), Phase::Commit) => {
            delta.push((
                release(
                    action_atoms(action)
                        .iter()
                        .enumerate()
                        .map(|(i, _)| Owner::Atom {
                            at: site.clone(),
                            index: i as u32,
                        }),
                ),
                -1,
            ));
        }
        _ => {}
    }
    (p, peak, d, delta)
}

fn check_entities(
    source: &Source,
    event: &Event,
    site: &Site,
    node: doc::Node<'_>,
    phase: Phase,
) -> Result<(), Refused> {
    let mut expected = Vec::new();
    let mut names = Vec::new();
    match (node, phase) {
        (doc::Node::Block(_), Phase::Enter) if *site == source.site => {
            names.extend(source.params.iter().map(|n| (site.clone(), n)))
        }
        (
            doc::Node::Expr(
                doc::Expr::List(_)
                | doc::Expr::Set(_)
                | doc::Expr::Map(_)
                | doc::Expr::Record(_)
                | doc::Expr::Closure(_),
            ),
            Phase::Commit,
        ) => expected.push(Owner::Entity(site.clone())),
        (doc::Node::Stmt(doc::Stmt::Let { name, .. }), Phase::Commit)
        | (doc::Node::Expr(doc::Expr::Let { name, .. }), Phase::Test) => {
            names.push((site.clone(), name))
        }
        (doc::Node::Stmt(doc::Stmt::For { binding, .. }), Phase::Test) => {
            names.push((child(site, 1), binding))
        }
        (doc::Node::Stmt(doc::Stmt::Try(t)), Phase::Catch) => {
            if let Some(c) = &t.catch {
                names.push((child(site, 1), &c.binding));
            }
        }
        _ => {}
    }
    for (declared, name) in names {
        if source.shared.get(&(declared.clone(), name.clone())) == Some(&true) {
            expected.push(Owner::Cell {
                declared,
                name: name.clone(),
            });
        }
    }
    if event.entities.as_ref() != expected {
        return Err(Refused::Retained);
    }
    Ok(())
}
