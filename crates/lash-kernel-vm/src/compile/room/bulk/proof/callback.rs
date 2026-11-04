//! Only source-authenticated call boundaries add callback value rules.
use super::*;

pub(super) fn handles(cx: &Compiler<'_>, event: &Event) -> bool {
    cx.callback.is_some_and(|program| {
        program.call().at().site == event.point.site
            && (program.call().at() == &event.point
                || matches!(event.point.phase, Phase::Open | Phase::Restore)
                || (program.call().root_arguments().is_some()
                    && event.point.phase == Phase::Commit))
    })
}

pub(super) fn compile_event(cx: &mut Compiler<'_>, event: &Event) -> Result<EventRule, Refused> {
    if cx.member_seen || !event.entities.is_empty() {
        return Err(Refused::Source);
    }
    let program = cx.callback.ok_or(Refused::Source)?;
    if event.point == *program.call().at()
        && let Some(atoms) = program.call().root_arguments()
    {
        if atoms.len() != program.arguments().len() {
            return Err(Refused::Source);
        }
        for (slot, atom) in program.arguments().iter().zip(atoms) {
            let value = match atom {
                region::ScalarInput::Literal(value) => cx.value(ValueRule::Literal(*value)),
                region::ScalarInput::Binding(binding) => {
                    let from = cx.entry(*binding)?;
                    cx.value(ValueRule::Copy(from))
                }
            };
            if cx.live.insert(*slot, value).is_some() {
                return Err(Refused::Retained);
            }
            cx.vacant.insert(*slot);
        }
    }
    let mut rule = EventRule {
        at: event.point.clone(),
        measures: Box::default(),
        call: None,
        condition: None,
        member: None,
    };
    let before = cx.live.clone();
    let mut measures = Vec::new();
    for measure in event.peak.measures().chain(event.delta.measures()) {
        if let Measure::Release(owners) | Measure::NativeRelease { owners, .. } = measure {
            let mut values = Vec::new();
            for owner in owners.iter() {
                if let Owner::Incoming(at) = owner {
                    values.push(*cx.callback_results.get(at).ok_or(Refused::Source)?);
                } else {
                    values.extend(cx.release(std::slice::from_ref(owner))?);
                }
            }
            let measure = match measure {
                Measure::NativeRelease { owners, .. } => Measure::Release(owners.clone()),
                _ => measure.clone(),
            };
            if !measures.iter().any(|(actual, _)| actual == &measure) {
                measures.push((measure, values.into_boxed_slice()));
            }
        }
    }
    let output = if event.point == *program.call().at() {
        let arguments = program
            .arguments()
            .iter()
            .map(|slot| cx.live.get(slot).copied().ok_or(Refused::Retained))
            .collect::<Result<Box<_>, _>>()?;
        let roles = program
            .call()
            .scope_roles()
            .map(|slot| {
                let value = match cx.live.get(&slot) {
                    Some(value) => *value,
                    None => cx.entry(slot)?,
                };
                Ok((slot, value))
            })
            .collect::<Result<Box<_>, Refused>>()?;
        let value = cx.value(ValueRule::Callback {
            call: Arc::clone(program.call()),
            at: event.point.clone(),
            arguments,
            roles,
        });
        if cx
            .callback_results
            .insert(event.point.site.clone(), value)
            .is_some()
        {
            return Err(Refused::Source);
        }
        let callee = event
            .peak
            .measures()
            .chain(event.delta.measures())
            .find_map(|measure| match measure {
                Measure::CallPeak { at, callee } | Measure::CallDelta { at, callee }
                    if *at == event.point.site =>
                {
                    Some(*callee)
                }
                _ => None,
            })
            .unwrap_or(CallSource::Dynamic);
        rule.call = Some((callee, value));
        if program.call().root_arguments().is_none() {
            for slot in program.call().consumed_roles() {
                cx.live.remove(&slot);
            }
        }
        event.output.as_ref().map(|_| value)
    } else if event.point.phase == Phase::Restore
        || (program.call().root_arguments().is_some() && event.point.phase == Phase::Commit)
    {
        Some(
            *cx.callback_results
                .get(&event.point.site)
                .ok_or(Refused::Source)?,
        )
    } else if event.point.phase == Phase::Open {
        let doc::Node::Expr(doc::Expr::Act(action)) = cx.node(&event.point)? else {
            return Err(Refused::Source);
        };
        let doc::ActKind::Call(doc::Callee::Value(name)) = &action.kind else {
            return Err(Refused::Source);
        };
        let name = name.clone();
        let slot = cx.binding(&event.point.site, &name)?;
        let input = cx.entry(slot)?;
        Some(cx.value(ValueRule::Copy(input)))
    } else {
        return Err(Refused::Source);
    };
    if program.call().root_arguments().is_some() && event.point.phase == Phase::Commit {
        for slot in program.call().consumed_roles() {
            cx.live.remove(&slot);
        }
    }
    if let Some(value) = output.filter(|_| !matches!(event.output, Some(Owner::Incoming(_)))) {
        let slot = cx.slot(event.output.as_ref().ok_or(Refused::Source)?)?;
        if cx.live.insert(slot, value).is_some() {
            return Err(Refused::Retained);
        }
        if !cx.entries.contains_key(&slot) {
            cx.vacant.insert(slot);
        }
    }
    for measure in event.peak.measures().chain(event.delta.measures()) {
        match measure {
            Measure::Local(owner) => {
                let fact = if let Owner::Incoming(at) = owner {
                    *cx.callback_results.get(at).ok_or(Refused::Source)?
                } else {
                    let slot = cx.slot(owner)?;
                    *cx.live
                        .get(&slot)
                        .or_else(|| before.get(&slot))
                        .ok_or(Refused::Retained)?
                };
                if !measures.iter().any(|(actual, _)| actual == measure) {
                    measures.push((measure.clone(), Box::new([fact])));
                }
            }
            Measure::Release(_)
            | Measure::NativeRelease { .. }
            | Measure::CallPeak { .. }
            | Measure::CallDelta { .. } => {}
            _ => return Err(Refused::Facts),
        }
    }
    rule.measures = measures.into();
    Ok(rule)
}
