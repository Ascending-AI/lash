//! Finite guarded source paths are compiled before binding any values. A
//! candidate's Boolean guards select one path; unsupported paths hand off.
use super::*;

pub(super) struct BodyPath {
    pub(super) region: super::nested::BodyRegion,
    pub(super) natives: Box<[Native]>,
    pub(super) charges: ComposedCharges,
}

struct Route<'a> {
    source: &'a Source,
    choices: &'a BTreeMap<Site, bool>,
    selected: Vec<Selection>,
    missing: Option<Site>,
    exe: &'a Executable,
    code: CodeId,
    context: &'a mut super::nested::CompileContext,
    calls: Vec<Arc<VerifiedCall>>,
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
    fn resume_call(
        &mut self,
        source: &Source,
        at: &Point,
        exit: ExitKind,
    ) -> Result<Option<CallCompletion>, Refused> {
        let Some(call) = super::nested::resolve(self.exe, self.code, at, self.context)? else {
            return Ok(None);
        };
        let completion = call.completion(source, at, exit)?;
        self.calls.push(call);
        Ok(Some(completion))
    }
    fn choose(&mut self, site: &Site) -> Result<bool, Refused> {
        let Some(&taken) = self.choices.get(site) else {
            self.missing = Some(site.clone());
            return Err(Refused::Path);
        };
        self.selected.push(Selection::Branch {
            at: site.clone(),
            taken,
        });
        Ok(taken)
    }
}

pub(super) fn compile(
    exe: &Executable,
    caller: CodeId,
    statement: StmtId,
    range: OpRange,
    callee: CodeId,
    context: &mut super::nested::CompileContext,
) -> Result<Box<[BodyPath]>, Refused> {
    let source = &exe.code(callee).source_room;
    let mut pending = vec![BTreeMap::new()];
    let mut paths = Vec::new();
    let mut budget = 4096_usize;
    while let Some(choices) = pending.pop() {
        budget = budget.checked_sub(1).ok_or(Refused::Path)?;
        let mut route = Route {
            source,
            choices: &choices,
            selected: vec![],
            missing: None,
            exe,
            code: callee,
            context,
            calls: Vec::new(),
        };
        let walked = evaluate::walk(source, &source.site, &mut route, 4096);
        if let Some(site) = route.missing {
            // Only scalar If paths are admitted. Loops and dynamic controls
            // cannot manufacture an unbounded template through this queue.
            let mut node = doc::Node::Block(&source.body);
            for index in &site.path[source.site.path.len()..] {
                node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
            }
            if !matches!(
                node,
                doc::Node::Stmt(doc::Stmt::If { .. }) | doc::Node::Expr(doc::Expr::If { .. })
            ) {
                continue;
            }
            for taken in [false, true] {
                let mut next = choices.clone();
                next.insert(site.clone(), taken);
                pending.push(next);
            }
            continue;
        }
        if !matches!(walked, Ok(ExitKind::Return | ExitKind::Normal)) {
            continue;
        }
        let path = (|| {
            let (region, natives) = if route.calls.is_empty() {
                let interval = source.compile_interval(&source.site, &route.selected, 4096)?;
                let scalar = if exe.code(callee).captures.is_empty() {
                    source.compile_scalar(interval, true)?
                } else {
                    source.compile_callback_body(interval)?
                };
                let natives = natives(exe, source, &scalar)?;
                (super::nested::BodyRegion::Scalar(Box::new(scalar)), natives)
            } else {
                (
                    super::nested::BodyRegion::Composed(Box::new(super::nested::Body::compile(
                        exe,
                        callee,
                        &route.selected,
                        &route.calls,
                    )?)),
                    Box::new([]) as Box<[Native]>,
                )
            };
            let charges =
                ComposedCharges::compile(exe, caller, statement, range, callee, &route.selected)?;
            Ok::<_, Refused>(BodyPath {
                region,
                natives,
                charges,
            })
        })();
        if let Ok(path) = path {
            paths.push(path);
        }
    }
    if paths.is_empty() {
        return Err(Refused::Source);
    }
    Ok(paths.into())
}

pub(super) fn natives(
    exe: &Executable,
    source: &Source,
    region: &ScalarRegion,
) -> Result<Box<[Native]>, Refused> {
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
            || args.len() != function.arity
            || !function.body_arity(args.len())
            || function.specialization.prepared.is_none()
        {
            return Err(Refused::Source);
        }
        let prior = region
            .interval
            .events()
            .position(|event| event.point == result.at)
            .and_then(|at| at.checked_sub(1))
            .and_then(|at| region.recovery.get(at));
        let inputs = (0..args.len())
            .map(|at| {
                let operand = slot(source, &Owner::Operand(result.at.site.child(at as u32)))?;
                prior
                    .and_then(|map| {
                        map.roles
                            .iter()
                            .find(|(candidate, _)| *candidate == operand)
                            .map(|(_, fact)| *fact)
                    })
                    .or_else(|| {
                        prior
                            .is_none()
                            .then(|| {
                                region
                                    .entries
                                    .iter()
                                    .find(|entry| entry.slot == operand)
                                    .map(|entry| entry.live.fact)
                            })
                            .flatten()
                    })
                    .ok_or(Refused::Exit)
            })
            .collect::<Result<Box<[_]>, _>>()?;
        natives.push(Native {
            at: result.at.clone(),
            lib,
            inputs,
            result: result.live.fact,
        });
    }
    Ok(natives.into())
}

pub(super) fn verify_coverage(call: &VerifiedCall) -> Result<(), Refused> {
    if call.paths.is_empty()
        || call.paths.iter().enumerate().any(|(index, path)| {
            !Arc::ptr_eq(path.region.source(), &call.source)
                || call.paths[..index]
                    .iter()
                    .any(|prior| prior.region.selections() == path.region.selections())
        })
    {
        return Err(Refused::Source);
    }
    Ok(())
}

pub(super) fn evaluate_native(
    native: &Native,
    exe: &Executable,
    arena: &Arena,
    values: &[Value],
) -> Result<Option<(Vec<Value>, Value)>, doc::heap::HeapFault> {
    Ok(evaluate_native_room(native, exe, arena, values)?.map(|(input, answer, _)| (input, answer)))
}

pub(super) fn evaluate_native_room(
    native: &Native,
    exe: &Executable,
    arena: &Arena,
    values: &[Value],
) -> Result<Option<(Vec<Value>, Value, CheckedRoom)>, doc::heap::HeapFault> {
    let Some(input) = native
        .inputs
        .iter()
        .map(|fact| values.get(fact.index()).copied())
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(None);
    };
    let function = exe.lib(native.lib);
    if function
        .header
        .signature
        .params
        .iter()
        .zip(&input)
        .any(|(parameter, value)| !compile::import_result_fits(&parameter.ty, *value))
    {
        return Ok(None);
    }
    let Some(answer) = function
        .specialization
        .prepared
        .as_ref()
        .and_then(|(entry, _)| entry.call(&input))
    else {
        return Ok(None);
    };
    if !super::scope::scalar(answer)
        || !compile::import_result_fits(&function.header.signature.result, answer)
    {
        return Ok(None);
    }
    let LibRun::Native {
        native: implementation,
        ..
    } = &function.run
    else {
        return Ok(None);
    };
    let contract = match implementation.resource_contract(doc::Args::new(
        &input,
        arena.objects(),
        &arena.semantics(),
    )) {
        Ok(Some(contract)) => contract,
        Err(doc::NativeError::Host(error)) => return Err(error),
        Err(doc::NativeError::Malformed(reason)) => {
            return Err(doc::heap::HeapFault::Malformed(reason.into()));
        }
        _ => return Ok(None),
    };
    let expected = doc::NativeResourceContract::value(
        doc::NativeResourceExit::Returned,
        arena.logical_local(&answer)?,
        doc::NativeResourceOrigin::Immutable,
    );
    if contract != expected {
        return Ok(None);
    }
    let Some(room) = CheckedRoom::native(function.specialization.function, &contract) else {
        return Ok(None);
    };
    Ok(Some((input, answer, room)))
}
