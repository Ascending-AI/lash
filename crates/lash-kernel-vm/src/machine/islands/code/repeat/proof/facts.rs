//! Actual kernel answers bind the immutable scalar and call facts.
use super::*;
impl Proof {
    fn native_room(
        &self,
        index: usize,
        exe: &Executable,
        arena: &Arena,
        entry: &[(Slot, Value)],
        actual: &Values,
    ) -> Result<Option<room::CheckedRoom>, HeapFault> {
        let native = &self.natives[index];
        let Some(arguments) = native
            .arguments
            .iter()
            .map(|value| self.value(*value, entry, actual))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(None);
        };
        let Some(result) = self.value(native.result, entry, actual) else {
            return Ok(None);
        };
        let function = exe.lib(native.lib);
        if !function
            .header
            .signature
            .params
            .iter()
            .zip(&arguments)
            .all(|(param, value)| {
                (param.optional && matches!(value, Value::Absent))
                    || crate::compile::import_result_fits(&param.ty, *value)
            })
            || !crate::compile::import_result_fits(&function.header.signature.result, result)
            || function
                .validate_known_scalar(arena, &arguments, result)
                .is_none()
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
        let query = implementation.resource_contract(lash_kernel_doc::Args::new(
            &arguments,
            arena.objects(),
            &arena.semantics(),
        ));
        let query = match query {
            Ok(None) => function.canonical_scalar_contract(arena, &arguments, result),
            other => other,
        };
        let contract = match query {
            Ok(Some(contract)) => contract,
            _ => return Ok(None),
        };
        if contract
            != lash_kernel_doc::NativeResourceContract::value(
                lash_kernel_doc::NativeResourceExit::Returned,
                arena.logical_local(&result)?,
                lash_kernel_doc::NativeResourceOrigin::Immutable,
            )
        {
            return Ok(None);
        }
        Ok(room::CheckedRoom::native(
            function.specialization.function,
            &contract,
        ))
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "one persistent source entry with changing kernel facts and scoped calls"
    )]
    pub(in crate::machine::islands) fn evaluate<'a>(
        &'a self,
        exe: &Executable,
        arena: &Arena,
        ledger: &Ledger,
        invocation: &Invocation,
        scope: Option<&CallScope<'_>>,
        entry: &[(Slot, Value)],
        actual: &Values,
    ) -> Result<Option<Evaluated<'a>>, HeapFault> {
        let mut values = Vec::new();
        let mut rooms = Vec::new();
        let mut candidates: Vec<ScopedCall<'a>> = Vec::new();
        for step in &self.steps {
            match *step {
                Step::Scalar(index) => {
                    let plan = &self.scalars[index];
                    let Some(facts) = plan
                        .values
                        .iter()
                        .map(|value| self.value(*value, entry, actual))
                        .collect::<Option<Vec<_>>>()
                    else {
                        return Ok(None);
                    };
                    let mut calls = Vec::new();
                    for native in &plan.natives {
                        let Some(room) = self.native_room(*native, exe, arena, entry, actual)?
                        else {
                            return Ok(None);
                        };
                        calls.push(room);
                    }
                    values.push(facts);
                    rooms.push(calls);
                }
                Step::Call(index) => {
                    let Some(scope) = scope else {
                        return Ok(None);
                    };
                    let call = &self.calls[index];
                    let Some(roles) = self.roles[index]
                        .iter()
                        .map(|(slot, at)| Some((*slot, self.value(*at, entry, actual)?)))
                        .collect::<Option<Vec<_>>>()
                    else {
                        return Ok(None);
                    };
                    let offset = match candidates.last() {
                        Some(previous) => match previous.next_offset() {
                            Some(offset) => offset,
                            None => return Ok(None),
                        },
                        None => scope.completed(),
                    };
                    let candidate = match call.call.evaluate_scoped(
                        exe,
                        call.call.callee(),
                        scope,
                        offset,
                        arena,
                        &roles,
                    ) {
                        Ok(Some(candidate)) => candidate,
                        _ => return Ok(None),
                    };
                    if self
                        .value(call.result, entry, actual)
                        .is_none_or(|expected| !expected.same(&candidate.result()))
                    {
                        return Ok(None);
                    }
                    if let Some(lib) = call.library
                        && let LibRun::Native {
                            native,
                            body: Some(_),
                        } = &exe.lib(lib).run
                    {
                        let Some(arguments) = call
                            .arguments
                            .iter()
                            .map(|at| self.value(*at, entry, actual))
                            .collect::<Option<Vec<_>>>()
                        else {
                            return Ok(None);
                        };
                        let result = candidate.result();
                        if exe
                            .lib(lib)
                            .validate_known_scalar(arena, &arguments, result)
                            .is_none()
                        {
                            return Ok(None);
                        }
                        let query = native.resource_contract(lash_kernel_doc::Args::new(
                            &arguments,
                            arena.objects(),
                            &arena.semantics(),
                        ));
                        let query = match query {
                            Ok(None) => exe
                                .lib(lib)
                                .canonical_scalar_contract(arena, &arguments, result),
                            other => other,
                        };
                        let contract = match query {
                            Ok(Some(contract)) => contract,
                            _ => return Ok(None),
                        };
                        if contract
                            != lash_kernel_doc::NativeResourceContract::value(
                                lash_kernel_doc::NativeResourceExit::Returned,
                                arena.logical_local(&result)?,
                                lash_kernel_doc::NativeResourceOrigin::Immutable,
                            )
                        {
                            return Ok(None);
                        }
                    }
                    candidates.push(candidate);
                }
            }
        }
        let scalars: Vec<_> = values
            .iter()
            .zip(&rooms)
            .map(|(values, calls)| ScalarRegionValues { values, calls })
            .collect();
        let calls: Vec<_> = candidates.iter().collect();
        let Some(effects) = self.recurrence.iteration.measure(
            exe,
            arena,
            ledger,
            invocation,
            IterationValues {
                entry,
                scalars: &scalars,
                calls: &calls,
            },
        )?
        else {
            return Ok(None);
        };
        Ok(Some(Evaluated {
            effects,
            candidates,
        }))
    }
}

// Physical writes must name the binding selected by the original source.
// The lifetime compiler's logical survivors cannot authenticate a different
// physical target merely because its holding has the same price.
pub(super) fn target(
    placement: &BindingPlacement,
    exe: &Executable,
    id: StmtId,
    physical: u32,
) -> Option<()> {
    let source = placement.source();
    let site = &exe.stmt(id).site;
    let commit = Point {
        site: site.clone(),
        phase: Phase::Commit,
    };
    let event = source.compile_event(&commit, room::ExitKind::Normal).ok()?;
    let mut owner = event.events().next()?.output.clone();
    if owner.is_none() {
        let mut node = lash_kernel_doc::Node::Block(&source.body);
        for index in &site.path[source.site.path.len()..] {
            node = *node.children().get(*index as usize)?;
        }
        let lash_kernel_doc::Node::Stmt(lash_kernel_doc::Stmt::Assign {
            place: lash_kernel_doc::Place::Variable(name),
            ..
        }) = node
        else {
            return None;
        };
        owner = Some(room::Owner::Visible {
            at: site.clone(),
            name: name.clone(),
        });
    }
    let slot = owner_slot(source, &owner?)?;
    (placement.physical(slot) == Some(physical) && placement.unshared(physical)).then_some(())
}
