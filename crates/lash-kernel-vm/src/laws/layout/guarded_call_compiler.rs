use super::*;

pub(super) fn helper_compilation() {
    nested_compilation();
    while_test_compilation();
    native_iteration_compilation();
    // The supplied call compiler must retain a guarded scalar helper's
    // source path before its body can be composed into a repeated region.
    for helper_body in [
        "return flat.add(a, b)",
        "if flat.lt(a, b) { return flat.add(a, b) } return flat.add(a, b)",
    ] {
        for call_form in 0..7 {
            let helper = super::super::embedder::fragment(&format!(
                "function helper(a: Any, b: Any) -> Any\nkernel 1\nmetering statements\nbody {{ {helper_body} }}"
            ));
            let answer = match call_form {
                3 => 6,
                5 => 2,
                _ => 1,
            };
            let update = match call_form {
                0 => "set i = flat.identity(invoke helper(i, 1))",
                1 => "set i = flat.identity(invoke helper(flat.identity(i), 1))",
                2 => "set i = invoke helper(i, 1)",
                3 => "set i = flat.add(5, invoke helper(i, 1))",
                4 => {
                    if helper_body.starts_with("if") {
                        "set i = flat.identity(body.guarded(i, 1))"
                    } else {
                        "set i = flat.identity(body.plain(i, 1))"
                    }
                }
                5 => "set i = invoke helper(i, 1) set i = invoke helper(i, 1)",
                _ => {
                    "set s = flat.add(s, flat.mul(i, i)) let previous = flat.identity(i) set i = invoke helper(previous, 1)"
                }
            };
            let extra_binding = if call_form == 6 { "let s = 0" } else { "" };
            let returned = if call_form == 6 { "(s, i)" } else { "i" };
            let caller = scalar_document(&format!(
                "let i = 0 {extra_binding} while flat.lt(i, 4) {{ {update} }} return {returned}"
            ));
            let source = format!("definition \"{helper}\"\n{caller}");
            let embedder = Embedder::with(
                &source,
                Setup {
                    probes: Some(body_helper_probes),
                    ..Setup::default()
                },
            );
            let (program, _) = embedder.program(true);
            let admitted = program
                .library
                .admit(&program.document, Default::default(), &program.selection)
                .unwrap_or_else(|_| panic!("helper source admitted"));
            let exe = admitted
                .executable(
                    &program.document,
                    &program.library,
                    &program.documents,
                    &program.functions,
                    &program.instances,
                )
                .unwrap_or_else(|_| panic!("helper executable"));
            let caller = exe
                .islands
                .plans
                .iter()
                .find(|plan| {
                    plan.body.statements.iter().any(|statement| {
                        matches!(statement.kind, crate::compile::islands::Kind::While { .. })
                    })
                })
                .unwrap();
            let statement = caller
                .body
                .statements
                .iter()
                .find(|statement| {
                    matches!(statement.kind, crate::compile::islands::Kind::While { .. })
                })
                .unwrap();
            let crate::compile::islands::Kind::While { body, .. } = &statement.kind else {
                unreachable!()
            };
            let id = body.statements[if call_form == 6 { 2 } else { 0 }].id;
            let (stream, stmt, _) = exe.statement(id);
            let pc = (stmt.ops.start..stmt.ops.end())
                .find(|pc| {
                    matches!(
                        stream.ops[*pc as usize],
                        lash_kernel_exec::Op::Nested { .. } | lash_kernel_exec::Op::Act
                    )
                })
                .unwrap();
            let first_unpaid = (stmt.ops.start..pc)
                .rev()
                .find(|at| {
                    let op = stream.ops[*at as usize];
                    !op.kind().in_region() && !matches!(op, lash_kernel_exec::Op::Region { .. })
                })
                .map_or(stmt.ops.start, |at| at + 1);
            let call = crate::compile::room::region::calls::VerifiedCall::compile_from(
                &exe,
                caller.code,
                id,
                pc,
                first_unpaid,
            );
            eprintln!(
                "loop helper {helper_body:?}: {:?}",
                call.as_ref().map(|_| "verified")
            );
            assert!(
                call.is_ok(),
                "a pure guarded helper must have source-call authority"
            );
            use crate::compile::room::{
                self,
                placement::BindingPlacement,
                region::{
                    ScalarRegionValues,
                    calls::scope::iteration::{IterationValues, VerifiedIteration},
                },
            };
            use lash_kernel_doc::Value;
            use lash_kernel_state::{arena::Arena, kernel_internals::logical::RootValue};
            let call = std::sync::Arc::new(call.unwrap());
            let mut compiled_calls = vec![std::sync::Arc::clone(&call)];
            if call_form == 5 {
                let second = body.statements[1].id;
                let (_, statement, _) = exe.statement(second);
                compiled_calls.push(std::sync::Arc::new(
                    crate::compile::room::region::calls::VerifiedCall::compile(
                        &exe,
                        caller.code,
                        second,
                        statement.ops.start,
                    )
                    .unwrap(),
                ));
            }
            let iteration =
                VerifiedIteration::compile_loop(&exe, caller.code, statement.id, &compiled_calls)
                    .unwrap();
            let composed = iteration.interval();
            let boundary = &composed.calls()[0];
            assert_eq!(
                boundary.entry(),
                (
                    call.at(),
                    if call_form == 4 {
                        room::ExitKind::Normal
                    } else {
                        room::ExitKind::Action
                    }
                )
            );
            assert!(std::sync::Arc::ptr_eq(boundary.callee(), call.source()));
            assert_eq!(
                boundary.leave(),
                &room::Point {
                    site: call.source().site.clone(),
                    phase: room::Phase::Leave
                }
            );
            assert_eq!(
                boundary.resume(),
                &room::Point {
                    site: call.at().site.clone(),
                    phase: if matches!(call_form, 2 | 5 | 6) {
                        room::Phase::Commit
                    } else {
                        room::Phase::Restore
                    }
                }
            );
            let from = &composed.events().next().unwrap().point;
            let through = &composed.events().last().unwrap().point;
            assert!(
                iteration
                    .source()
                    .compile_between(from, through, composed.selections(), 4096)
                    .is_err(),
                "ordinary intervals must still stop at the genuine Action exit"
            );
            let mut omitted = composed.selections().to_vec();
            omitted.retain(|selection| !matches!(selection, room::Selection::Event { at, .. } if at == boundary.resume()));
            assert!(
                iteration
                    .source()
                    .compile_composed_between(
                        &exe,
                        caller.code,
                        from,
                        through,
                        &omitted,
                        &compiled_calls
                    )
                    .is_err(),
                "a caller resume cannot disappear from a composed source path"
            );
            let mut arena = Arena::new();
            let heap = embedder.machine.source_law_heap();
            let store = heap.logical_ledger();
            let mut ledger = store.borrow_mut();
            let initial = Value::Int(0);
            let bindings: Vec<_> = iteration
                .entries()
                .iter()
                .map(|slot| (*slot, RootValue::Value(&initial)))
                .collect();
            let (invocation, entered) = ledger
                .prepare_enter(
                    lash_kernel_doc::TaskId::MAIN,
                    &mut arena,
                    heap.pages(),
                    &[],
                    &bindings,
                )
                .unwrap();
            ledger
                .commit(entered.admit(u64::MAX).ok().unwrap())
                .unwrap();
            let placement = BindingPlacement::compile(&exe, caller.code).unwrap();
            let mut physical: Vec<_> = (0..exe.code(caller.code).slots.len())
                .map(|_| crate::machine::SlotState::Empty)
                .collect();
            for slot in iteration.entries() {
                physical[placement.physical(*slot).unwrap() as usize] =
                    crate::machine::SlotState::Value(initial);
            }
            let activation = placement
                .capture(caller.code, &invocation, &ledger, heap, &physical)
                .unwrap()
                .unwrap();
            drop(ledger);
            let frontier = scope_frontier(
                &exe,
                caller.code,
                &call,
                &store.borrow(),
                heap,
                &invocation,
                program.link,
            );
            let ledger = store.borrow();
            let reservation = ledger
                .reserve_invocations(lash_kernel_doc::TaskId::MAIN, 4)
                .unwrap();
            let scope = call
                .bind_scope(
                    &exe,
                    frontier.call_entry().unwrap(),
                    call.callee(),
                    &ledger,
                    &invocation,
                    &activation,
                    reservation,
                )
                .unwrap();
            let roles: Vec<_> = call
                .scope_roles()
                .zip(
                    [
                        initial,
                        Value::Int(1),
                        if call_form == 6 {
                            initial
                        } else {
                            Value::Int(5)
                        },
                    ]
                    .into_iter()
                    .chain(std::iter::repeat(initial)),
                )
                .collect();
            let candidate = call
                .evaluate_scoped(&exe, call.callee(), &scope, 0, &arena, &roles)
                .unwrap()
                .unwrap();
            let second_candidate = if call_form == 5 {
                let second = &compiled_calls[1];
                let roles: Vec<_> = second
                    .scope_roles()
                    .zip([Value::Int(1), Value::Int(1)])
                    .collect();
                Some(
                    second
                        .evaluate_scoped(&exe, second.callee(), &scope, 1, &arena, &roles)
                        .unwrap()
                        .unwrap(),
                )
            } else {
                None
            };
            let candidates: Vec<_> = std::iter::once(&candidate)
                .chain(second_candidate.iter())
                .collect();
            let mut scalar_values = Vec::new();
            let mut scalar_rooms = Vec::new();
            let binding = |name: &str| {
                let binding = exe
                    .code(caller.code)
                    .slots
                    .iter()
                    .find(|binding| binding.name.as_str() == name)?;
                match iteration.source().owner_slots().unwrap().root(
                    &room::Owner::Binding {
                        declared: binding.declared.clone(),
                        name: binding.name.clone(),
                    },
                    lash_kernel_doc::TaskId::MAIN,
                    0,
                )? {
                    lash_kernel_state::kernel_internals::logical::Owner::Value { slot, .. } => {
                        Some(slot)
                    }
                    _ => None,
                }
            };
            let previous = binding("previous");
            for (region_index, region) in iteration.scalars().enumerate() {
                let mut values = vec![Value::Null; region.fact_count()];
                for (slot, fact) in region.entries() {
                    values[fact.index()] = if previous == Some(slot) {
                        initial
                    } else if iteration.entries().contains(&slot) {
                        Value::Int(if call_form == 5 && region_index == 2 {
                            1
                        } else {
                            0
                        })
                    } else {
                        let value = compiled_calls
                            .iter()
                            .enumerate()
                            .find_map(|(index, call)| {
                                let owner = iteration
                                    .source()
                                    .owner_slots()
                                    .unwrap()
                                    .root(
                                        &room::Owner::Operand(call.at().site.clone()),
                                        lash_kernel_doc::TaskId::MAIN,
                                        0,
                                    )
                                    .unwrap();
                                let lash_kernel_state::kernel_internals::logical::Owner::Value {
                                    slot: result,
                                    ..
                                } = owner
                                else {
                                    unreachable!()
                                };
                                (slot == result).then_some(index as i64 + 1)
                            })
                            .unwrap_or(5);
                        Value::Int(value)
                    };
                }
                for (at, fact) in region.results() {
                    let mut node = lash_kernel_doc::Node::Block(&iteration.source().body);
                    for index in &at.site.path[iteration.source().site.path.len()..] {
                        node = *node.children().get(*index as usize).unwrap();
                    }
                    values[fact.index()] = match node {
                        lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Literal(
                            lash_kernel_doc::Literal::Int(value),
                        )) => Value::Int(value.word().unwrap()),
                        lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Variable(_)) => initial,
                        lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Call { .. })
                            if call_form == 6
                                && at
                                    .site
                                    .path
                                    .starts_with(&exe.stmt(statement.id).site.child(1).path) =>
                        {
                            initial
                        }
                        lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Call {
                            args, ..
                        }) if args.len() == 2
                            && !at.site.path.starts_with(&exe.stmt(id).site.path) =>
                        {
                            Value::Bool(true)
                        }
                        lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Call { .. }) => {
                            if at.site.path.starts_with(&call.at().site.path) {
                                initial
                            } else {
                                Value::Int(answer)
                            }
                        }
                        _ => panic!("scalar loop source result"),
                    };
                }
                let mut rooms = Vec::new();
                for event in region.interval().events() {
                    for measure in event.peak.measures() {
                        if let room::Measure::CallPeak {
                            callee: room::CallSource::Native(function),
                            ..
                        } = measure
                        {
                            let lib = exe.lib(exe.library_lib(function).unwrap());
                            let crate::compile::LibRun::Native { native, .. } = &lib.run else {
                                panic!("scalar native");
                            };
                            let input = if lib.arity == 2 {
                                if call_form == 6
                                    && event
                                        .point
                                        .site
                                        .path
                                        .starts_with(&exe.stmt(statement.id).site.child(1).path)
                                {
                                    vec![initial, initial]
                                } else if event.point.site.path.starts_with(&exe.stmt(id).site.path)
                                {
                                    vec![Value::Int(5), Value::Int(1)]
                                } else {
                                    vec![initial, Value::Int(4)]
                                }
                            } else {
                                vec![if call_form == 6
                                    || event.point.site.path.starts_with(&call.at().site.path)
                                {
                                    initial
                                } else {
                                    Value::Int(1)
                                }]
                            };
                            let contract = native
                                .resource_contract(lash_kernel_doc::Args::new(
                                    &input,
                                    arena.objects(),
                                    &arena.semantics(),
                                ))
                                .unwrap()
                                .unwrap();
                            rooms.push(room::CheckedRoom::native(*function, &contract).unwrap());
                        }
                    }
                }
                scalar_values.push(values);
                scalar_rooms.push(rooms);
            }
            let facts: Vec<_> = scalar_values
                .iter()
                .zip(&scalar_rooms)
                .map(|(values, calls)| ScalarRegionValues { values, calls })
                .collect();
            let entry: Vec<_> = iteration
                .entries()
                .iter()
                .map(|slot| (*slot, initial))
                .collect();
            let effects = iteration
                .measure(
                    &exe,
                    &arena,
                    &ledger,
                    &invocation,
                    IterationValues {
                        entry: &entry,
                        scalars: &facts,
                        calls: &candidates,
                    },
                )
                .unwrap()
                .unwrap();
            assert_eq!(
                effects.room().retained_delta,
                1,
                "the source assignment replaces zero by one"
            );
            if call_form == 6 {
                assert_eq!(effects.survivors().len(), 2);
                let i = binding("i").unwrap();
                assert!(
                    effects
                        .survivors()
                        .iter()
                        .any(|(slot, value)| *slot == i && value.same(&Value::Int(answer)))
                );
                assert!(
                    !effects
                        .survivors()
                        .iter()
                        .any(|(slot, _)| Some(*slot) == previous),
                    "the source body Leave must release its local copy"
                );
                let mut wrong = scalar_values.clone();
                let (region_index, fact) = iteration
                    .scalars()
                    .enumerate()
                    .find_map(|(index, region)| {
                        region
                            .entries()
                            .find(|(slot, _)| Some(*slot) == previous)
                            .map(|(_, fact)| (index, fact))
                    })
                    .expect("the suffix must authenticate the carried local binding");
                wrong[region_index][fact.index()] = Value::Int(1);
                let wrong_facts: Vec<_> = wrong
                    .iter()
                    .zip(&scalar_rooms)
                    .map(|(values, calls)| ScalarRegionValues { values, calls })
                    .collect();
                assert!(
                    iteration
                        .measure(
                            &exe,
                            &arena,
                            &ledger,
                            &invocation,
                            IterationValues {
                                entry: &entry,
                                scalars: &wrong_facts,
                                calls: &candidates
                            }
                        )
                        .unwrap()
                        .is_none(),
                    "a carried scope binding must keep its actual source value"
                );
            } else {
                assert!(
                    matches!(effects.survivors(), [(slot, value)] if *slot == iteration.entries()[0] && value.same(&Value::Int(answer)))
                );
            }
            assert_eq!(
                effects.invocation_offsets(),
                Some((0, if call_form == 5 { 2 } else { 1 }))
            );
            assert_eq!(
                iteration.reservation_width(),
                Some(if call_form == 5 { 2 } else { 1 })
            );
            assert_eq!(
                effects.completion_offsets(),
                if call_form == 5 {
                    &[0, 1][..]
                } else {
                    &[0][..]
                }
            );
            let expected_fee = if helper_body.starts_with("if") {
                49
            } else {
                38
            } + if call_form == 1 { 1 + 8 + 1 } else { 0 }
                - if matches!(call_form, 2 | 5 | 6) {
                    12
                } else {
                    0
                }
                - if call_form == 4 && helper_body.starts_with("if") {
                    11
                } else {
                    0
                }
                + if call_form == 5 {
                    if helper_body.starts_with("if") {
                        28
                    } else {
                        16
                    }
                } else {
                    0
                }
                + if call_form == 6 {
                    // Six forms and two scalar fees for the sum, followed
                    // by three forms and the identity fee for the copy.
                    6 + 2 * (3 + 1 + 1 + 1) + (3 + 8 + 1)
                } else {
                    0
                };
            // The source interpreter supplies an independent meter oracle.
            // The control begins at the post-iteration value, so it has the
            // identical false test/return suffix and no executed iteration.
            let source_fee = |initial| {
                let caller = scalar_document(&format!(
                    "let i = {initial} {extra_binding} while flat.lt(i, 1) {{ {update} }} return {returned}"
                ));
                let source = format!("definition \"{helper}\"\n{caller}");
                let mut run = Embedder::with(
                    &source,
                    Setup {
                        probes: Some(body_helper_probes),
                        ..Setup::default()
                    },
                );
                let tape = crate::ChargeTape::new();
                run.machine.record_charges(tape.clone());
                let end = loop {
                    run.machine.force_source_execution();
                    match run.run() {
                        Step::Ended(end) => break end,
                        Step::Slice => {}
                        Step::Parked(_) => panic!("pure source oracle must not park"),
                    }
                };
                (
                    format!("{:?}", result(end)),
                    run.machine.meters().charged.unwrap(),
                )
            };
            let executed = source_fee(0);
            let suffix = source_fee(answer);
            assert_eq!(executed.0, suffix.0);
            let reference_fee = executed.1 - suffix.1;
            assert_eq!(
                expected_fee, reference_fee,
                "hand-derived forms and actual source charges agree"
            );
            assert_eq!(
                effects
                    .debts()
                    .iter()
                    .map(|debt| debt.units().unwrap())
                    .sum::<u64>(),
                expected_fee
            );
            let tape = crate::observe::ChargeTape::new();
            for debt in effects.debts() {
                debt.record(&tape);
            }
            assert_eq!(
                tape.steps().iter().map(|step| step.units()).sum::<u64>(),
                expected_fee
            );
            assert_eq!(
                scope.completed(),
                0,
                "a measured iteration cannot consume its prospective invocation"
            );
            if let Some(second) = second_candidate.as_ref() {
                let second_call = &compiled_calls[1];
                let roles: Vec<_> = second_call
                    .scope_roles()
                    .zip([Value::Int(1), Value::Int(1)])
                    .collect();
                let duplicate = second_call
                    .evaluate_scoped(&exe, second_call.callee(), &scope, 0, &arena, &roles)
                    .unwrap()
                    .unwrap();
                assert_eq!(second.invocation().id(), candidate.invocation().id() + 1);
                assert!(
                    iteration
                        .measure(
                            &exe,
                            &arena,
                            &ledger,
                            &invocation,
                            IterationValues {
                                entry: &entry,
                                scalars: &facts,
                                calls: &[&candidate, &duplicate]
                            }
                        )
                        .unwrap()
                        .is_none(),
                    "completed offsets must be exact and ordered before iteration admission"
                );
            }
            if call_form == 3 {
                let mut wrong_roles = roles.clone();
                wrong_roles.last_mut().unwrap().1 = Value::Int(6);
                let wrong_saved = call
                    .evaluate_scoped(&exe, call.callee(), &scope, 0, &arena, &wrong_roles)
                    .unwrap()
                    .unwrap();
                assert!(
                    iteration
                        .measure(
                            &exe,
                            &arena,
                            &ledger,
                            &invocation,
                            IterationValues {
                                entry: &entry,
                                scalars: &facts,
                                calls: &[&wrong_saved]
                            }
                        )
                        .unwrap()
                        .is_none(),
                    "an equal-price saved caller operand cannot be substituted in an iteration"
                );
            }
            let mut wrong = scalar_values.clone();
            let last = iteration.scalars().last().unwrap();
            let output = last
                .results()
                .next()
                .map(|(_, fact)| fact)
                .or_else(|| {
                    last.entries()
                        .find(|(slot, _)| !iteration.entries().contains(slot))
                        .map(|(_, fact)| fact)
                })
                .unwrap();
            wrong.last_mut().unwrap()[output.index()] = Value::Int(if answer == 2 { 3 } else { 2 });
            let wrong_facts: Vec<_> = wrong
                .iter()
                .zip(&scalar_rooms)
                .map(|(values, calls)| ScalarRegionValues { values, calls })
                .collect();
            assert!(
                iteration
                    .measure(
                        &exe,
                        &arena,
                        &ledger,
                        &invocation,
                        IterationValues {
                            entry: &entry,
                            scalars: &wrong_facts,
                            calls: &candidates
                        },
                    )
                    .unwrap()
                    .is_none(),
                "same-price native continuation answers must authenticate"
            );
        }
    }
}

fn nested_compilation() {
    use crate::compile::room::{placement::BindingPlacement, region::calls::VerifiedCall};
    use lash_kernel_doc::Value;
    use lash_kernel_state::arena::Arena;
    for (name, argument, answer, width) in [
        ("body.chain", 5, 5, 2),
        ("body.guarded_chain", 0, 0, 1),
        ("body.charged_chain", 5, 10, 2),
    ] {
        let invoke = if name == "body.charged_chain" {
            "invoke "
        } else {
            ""
        };
        let text = scalar_document(&format!(
            "let answer = {invoke}{name}({argument}) return answer"
        ));
        let run = Embedder::with(
            &text,
            Setup {
                probes: Some(body_helper_probes),
                ..Setup::default()
            },
        );
        let (program, _) = run.program(true);
        let admitted = program
            .library
            .admit(&program.document, Default::default(), &program.selection)
            .unwrap_or_else(|_| panic!("nested source admitted"));
        let exe = admitted
            .executable(
                &program.document,
                &program.library,
                &program.documents,
                &program.functions,
                &program.instances,
            )
            .unwrap_or_else(|_| panic!("nested executable"));
        let caller = exe
            .islands
            .plans
            .iter()
            .find(|plan| {
                let source = &exe.code(plan.code).source_room;
                source.params.is_empty()
                    && source.body.len() == 2
                    && matches!(source.body[0], lash_kernel_doc::Stmt::Let { .. })
                    && source.site.unit != lash_kernel_value::sites::Unit::Main
            })
            .unwrap();
        let statement = caller.body.statements[0].id;
        let (stream, stmt, _) = exe.statement(statement);
        let pc = (stmt.ops.start..stmt.ops.end())
            .find(|pc| {
                matches!(
                    stream.ops[*pc as usize],
                    lash_kernel_exec::Op::Nested { .. } | lash_kernel_exec::Op::Act
                )
            })
            .unwrap();
        let call = VerifiedCall::compile(&exe, caller.code, statement, pc)
            .expect("finite pure nested bodies must compose");
        assert_eq!(call.reservation_width(), Some(width));
        let mut arena = Arena::new();
        let heap = run.machine.source_law_heap();
        let store = heap.logical_ledger();
        let mut ledger = store.borrow_mut();
        let (invocation, entered) = ledger
            .prepare_enter(
                lash_kernel_doc::TaskId::MAIN,
                &mut arena,
                heap.pages(),
                &[],
                &[],
            )
            .unwrap();
        ledger
            .commit(entered.admit(u64::MAX).ok().unwrap())
            .unwrap();
        let placement = BindingPlacement::compile(&exe, caller.code).unwrap();
        let physical: Vec<_> = (0..exe.code(caller.code).slots.len())
            .map(|_| crate::machine::SlotState::Empty)
            .collect();
        let activation = placement
            .capture(caller.code, &invocation, &ledger, heap, &physical)
            .unwrap()
            .unwrap();
        drop(ledger);
        let frontier = scope_frontier(
            &exe,
            caller.code,
            &call,
            &store.borrow(),
            heap,
            &invocation,
            program.link,
        );
        let ledger = store.borrow();
        let range = ledger
            .reserve_invocations(lash_kernel_doc::TaskId::MAIN, width)
            .unwrap();
        let mut scope = call
            .bind_scope(
                &exe,
                frontier.call_entry().unwrap(),
                call.callee(),
                &ledger,
                &invocation,
                &activation,
                range,
            )
            .unwrap();
        let roles: Vec<_> = call
            .scope_roles()
            .map(|slot| (slot, Value::Int(argument)))
            .collect();
        let candidate = call
            .evaluate_scoped(&exe, call.callee(), &scope, 0, &arena, &roles)
            .unwrap()
            .expect("nested source/result/native contracts must authenticate");
        assert!(candidate.result().same(&Value::Int(answer)));
        assert_eq!(candidate.completed_invocations(), width);
        assert_eq!(candidate.next_offset(), Some(width));
        if width == 2 {
            assert_eq!(
                candidate
                    .completions()
                    .iter()
                    .map(|completion| completion.offset())
                    .collect::<Vec<_>>(),
                [1, 0]
            );
            let child = &candidate.completions()[0];
            assert_eq!(child.caller(), candidate.invocation());
            assert_eq!(child.invocation().id(), candidate.invocation().id() + 1);
            assert!(!std::sync::Arc::ptr_eq(child.source(), call.source()));
            assert_eq!(child.leave().site, child.source().site);
            assert_eq!(
                child.saved_roles().len(),
                if name == "body.charged_chain" { 2 } else { 1 }
            );
            assert!(
                child
                    .saved_roles()
                    .iter()
                    .all(|(_, value)| value.same(&Value::Int(argument)))
            );
        } else {
            let wrong: Vec<_> = call
                .scope_roles()
                .map(|slot| (slot, Value::Int(5)))
                .collect();
            assert!(
                call.evaluate_scoped(&exe, call.callee(), &scope, 0, &arena, &wrong)
                    .unwrap()
                    .is_none(),
                "an input reaching a shared closure path must still hand off"
            );
        }
        let debt = candidate.debt().unwrap();
        if name != "body.charged_chain" {
            assert_eq!(
                debt.units(),
                3 + 7 + 2 * (1 + u64::from(argument != 0)),
                "formula-metered nested bodies keep the original caller forms and outer formula"
            );
        }
        let tape = crate::ChargeTape::new();
        debt.record(&tape);
        assert_eq!(
            tape.steps()
                .iter()
                .map(|weight| weight.units())
                .sum::<u64>(),
            debt.units()
        );
        let source_run = |source: &str| {
            let mut run = Embedder::with(
                source,
                Setup {
                    probes: Some(body_helper_probes),
                    ..Setup::default()
                },
            );
            let tape = crate::ChargeTape::new();
            run.machine.record_charges(tape.clone());
            let end = loop {
                run.machine.force_source_execution();
                match run.run() {
                    Step::Ended(end) => break end,
                    Step::Slice => {}
                    Step::Parked(_) => panic!("pure nested source must not park"),
                }
            };
            (
                format!("{:?}", result(end)),
                run.machine.meters().charged.unwrap(),
                tape.steps(),
            )
        };
        let actual = source_run(&text);
        let control = source_run(&scalar_document(&format!(
            "let answer = {answer} return answer"
        )));
        assert_eq!(actual.0, control.0);
        assert_eq!(
            debt.units(),
            actual.1 - control.1 + 2,
            "composed debt replaces the control's literal and Let closes"
        );
        let composed = tape.steps();
        assert!(
            control.2.iter().enumerate().any(|(index, weight)| {
                weight.units() == 1
                    && control
                        .2
                        .get(index + 1)
                        .is_some_and(|next| next.units() == 1)
                    && control.2[..index]
                        .iter()
                        .chain(composed.iter())
                        .chain(control.2[index + 2..].iter())
                        .copied()
                        .collect::<Vec<_>>()
                        == actual.2
            }),
            "nested debt must retain the exact source tape order"
        );
        assert_eq!(
            scope.completed(),
            0,
            "evaluation must not consume any reserved invocation"
        );
        assert!(
            call.evaluate_scoped(&exe, call.callee(), &scope, width, &arena, &roles)
                .unwrap()
                .is_none(),
            "an exhausted range cannot mint another call"
        );
        if width > 1 {
            let short = ledger
                .reserve_invocations(lash_kernel_doc::TaskId::MAIN, width - 1)
                .unwrap();
            let short = call
                .bind_scope(
                    &exe,
                    frontier.call_entry().unwrap(),
                    call.callee(),
                    &ledger,
                    &invocation,
                    &activation,
                    short,
                )
                .unwrap();
            assert!(
                call.evaluate_scoped(&exe, call.callee(), &short, 0, &arena, &roles)
                    .unwrap()
                    .is_none(),
                "all nested IDs must fit before admitting any candidate"
            );
            assert_eq!(short.completed(), 0);
        }
        let abandoned = call
            .evaluate_scoped(&exe, call.callee(), &scope, 0, &arena, &roles)
            .unwrap()
            .unwrap();
        drop(abandoned);
        assert_eq!(
            scope.completed(),
            0,
            "abandoning a candidate cannot consume IDs"
        );
        scope.complete(candidate).unwrap();
        assert_eq!(scope.completed(), width);
        assert_eq!(
            scope.finish(frontier.call_entry().unwrap()).unwrap().1,
            width
        );
    }
}

fn while_test_compilation() {
    use crate::compile::room::{
        ExitKind, Phase,
        region::calls::{VerifiedCall, scope::iteration::VerifiedIteration},
    };
    let text = scalar_document("let i = 0 while body.lt(i, 4) { set i = flat.add(i, 1) } return i");
    let (program, _) = Embedder::with(
        &text,
        Setup {
            probes: Some(body_helper_probes),
            ..Setup::default()
        },
    )
    .program(true);
    let admitted = program
        .library
        .admit(&program.document, Default::default(), &program.selection)
        .unwrap_or_else(|_| panic!("while-test source admitted"));
    let exe = admitted
        .executable(
            &program.document,
            &program.library,
            &program.documents,
            &program.functions,
            &program.instances,
        )
        .unwrap_or_else(|_| panic!("while-test executable"));
    let caller = exe
        .islands
        .plans
        .iter()
        .find(|plan| {
            plan.body.statements.iter().any(|statement| {
                matches!(statement.kind, crate::compile::islands::Kind::While { .. })
            })
        })
        .unwrap();
    let id = caller
        .body
        .statements
        .iter()
        .find(|statement| matches!(statement.kind, crate::compile::islands::Kind::While { .. }))
        .unwrap()
        .id;
    let (stream, statement, _) = exe.statement(id);
    let crate::compile::Form::While { test, body, .. } = statement.form else {
        unreachable!()
    };
    let pc = (test.start..test.end())
        .find(|pc| {
            matches!(
                stream.ops[*pc as usize],
                lash_kernel_exec::Op::Nested { .. }
            )
        })
        .unwrap();
    assert!(
        pc >= statement.ops.end(),
        "the canonical test is separate from the while header"
    );
    let first_unpaid = (test.start..pc)
        .rev()
        .find(|at| {
            let op = stream.ops[*at as usize];
            !op.kind().in_region() && !matches!(op, lash_kernel_exec::Op::Region { .. })
        })
        .map_or(test.start, |at| at + 1);
    let call = std::sync::Arc::new(
        VerifiedCall::compile_from(&exe, caller.code, id, pc, first_unpaid)
            .expect("a call in the canonical while test must have source authority"),
    );
    call.verify(&exe).unwrap();
    let complete = VerifiedCall::compile(&exe, caller.code, id, pc)
        .expect("a complete while-test call starts at the canonical test fence");
    assert_eq!(complete.at(), call.at());
    assert!(std::sync::Arc::ptr_eq(complete.source(), call.source()));
    assert!(
        VerifiedCall::compile_from(&exe, caller.code, id, pc, statement.ops.start).is_err(),
        "the header cannot replace the test's unpaid fence"
    );
    assert!(
        VerifiedCall::compile(&exe, caller.code, id, statement.ops.start).is_err(),
        "a while header is not a call"
    );
    assert!(
        VerifiedCall::compile_from(
            &exe,
            caller.code,
            exe.block(body).stmts[0],
            pc,
            first_unpaid
        )
        .is_err(),
        "a condition call cannot belong to a body statement"
    );
    let iteration =
        VerifiedIteration::compile_loop(&exe, caller.code, id, &[std::sync::Arc::clone(&call)])
            .unwrap();
    assert_eq!(iteration.calls().count(), 1);
    let boundary = &iteration.interval().calls()[0];
    assert_eq!(boundary.entry(), (call.at(), ExitKind::Normal));
    assert_eq!(boundary.resume().phase, Phase::Restore);
    assert!(std::sync::Arc::ptr_eq(boundary.callee(), call.source()));
    assert!(!std::sync::Arc::ptr_eq(
        boundary.callee(),
        iteration.source()
    ));
    assert_eq!(boundary.leave().phase, Phase::Leave);
    assert_eq!(boundary.leave().site, call.source().site);
    let interval = iteration.interval();
    assert!(
        iteration
            .source()
            .compile_between(
                &interval.events().next().unwrap().point,
                &interval.events().last().unwrap().point,
                interval.selections(),
                4096
            )
            .is_err(),
        "ordinary intervals must retain the body-call handoff"
    );
}

fn native_iteration_compilation() {
    use crate::compile::room::{
        Measure,
        region::{
            ScalarRegionValues,
            calls::scope::iteration::{IterationValues, VerifiedIteration},
        },
    };
    use lash_kernel_doc::Value;
    use lash_kernel_state::{arena::Arena, kernel_internals::logical::RootValue};
    let text = scalar_document("let i = 0 while flat.lt(i, 4) { set i = flat.add(i, 1) } return i");
    let embedder = Embedder::with(
        &text,
        Setup {
            probes: Some(scalar_probes),
            ..Setup::default()
        },
    );
    let (program, _) = embedder.program(true);
    let admitted = program
        .library
        .admit(&program.document, Default::default(), &program.selection)
        .unwrap_or_else(|_| panic!("native iteration source admitted"));
    let exe = admitted
        .executable(
            &program.document,
            &program.library,
            &program.documents,
            &program.functions,
            &program.instances,
        )
        .unwrap_or_else(|_| panic!("native iteration executable"));
    let caller = exe
        .islands
        .plans
        .iter()
        .find(|plan| {
            plan.body.statements.iter().any(|statement| {
                matches!(statement.kind, crate::compile::islands::Kind::While { .. })
            })
        })
        .unwrap();
    let statement = caller
        .body
        .statements
        .iter()
        .find(|statement| matches!(statement.kind, crate::compile::islands::Kind::While { .. }))
        .unwrap();
    let iteration = VerifiedIteration::compile_loop(&exe, caller.code, statement.id, &[])
        .expect("native-only iterations must use the same verified composition authority");
    let region = iteration.scalars().next().unwrap();
    let mut arena = Arena::new();
    let heap = embedder.machine.source_law_heap();
    let mut ledger = lash_kernel_state::kernel_internals::logical::Ledger::fresh();
    let initial = Value::Int(0);
    let bindings: Vec<_> = iteration
        .entries()
        .iter()
        .map(|slot| (*slot, RootValue::Value(&initial)))
        .collect();
    let (invocation, entered) = ledger
        .prepare_enter(
            lash_kernel_doc::TaskId::MAIN,
            &mut arena,
            heap.pages(),
            &[],
            &bindings,
        )
        .unwrap();
    ledger
        .commit(entered.admit(u64::MAX).ok().unwrap())
        .unwrap();
    let body = exe.stmt(statement.id).site.child(1);
    let mut values = vec![Value::Null; region.fact_count()];
    for (_, fact) in region.entries() {
        values[fact.index()] = initial;
    }
    for (at, fact) in region.results() {
        let mut node = lash_kernel_doc::Node::Block(&iteration.source().body);
        for index in &at.site.path[iteration.source().site.path.len()..] {
            node = *node.children().get(*index as usize).unwrap();
        }
        values[fact.index()] = match node {
            lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Literal(
                lash_kernel_doc::Literal::Int(value),
            )) => Value::Int(value.word().unwrap()),
            lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Variable(_)) => initial,
            lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Call { .. })
                if at.site.path.starts_with(&body.path) =>
            {
                Value::Int(1)
            }
            lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Call { .. }) => Value::Bool(true),
            _ => panic!("native iteration scalar result"),
        };
    }
    let mut rooms = Vec::new();
    for event in region.interval().events() {
        for measure in event.peak.measures() {
            if let Measure::CallPeak {
                callee: crate::compile::room::CallSource::Native(function),
                ..
            } = measure
            {
                let crate::compile::LibRun::Native { native, .. } =
                    &exe.lib(exe.library_lib(function).unwrap()).run
                else {
                    unreachable!()
                };
                let input = [
                    initial,
                    Value::Int(if event.point.site.path.starts_with(&body.path) {
                        1
                    } else {
                        4
                    }),
                ];
                let contract = native
                    .resource_contract(lash_kernel_doc::Args::new(
                        &input,
                        arena.objects(),
                        &arena.semantics(),
                    ))
                    .unwrap()
                    .unwrap();
                rooms
                    .push(crate::compile::room::CheckedRoom::native(*function, &contract).unwrap());
            }
        }
    }
    let entry: Vec<_> = iteration
        .entries()
        .iter()
        .map(|slot| (*slot, initial))
        .collect();
    let facts = [ScalarRegionValues {
        values: &values,
        calls: &rooms,
    }];
    let effects = iteration
        .measure(
            &exe,
            &arena,
            &ledger,
            &invocation,
            IterationValues {
                entry: &entry,
                scalars: &facts,
                calls: &[],
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(effects.invocation_offsets(), None);
    assert_eq!(iteration.reservation_width(), Some(0));
    assert!(effects.completion_offsets().is_empty());
    assert_eq!(effects.room().retained_delta, 1);
    assert!(
        matches!(effects.survivors(), [(slot, value)] if *slot == iteration.entries()[0] && value.same(&Value::Int(1)))
    );
    assert!(iteration.calls().next().is_none());
    let ordinary = iteration
        .source()
        .compile_between(
            &iteration.interval().events().next().unwrap().point,
            &iteration.interval().events().last().unwrap().point,
            iteration.interval().selections(),
            4096,
        )
        .unwrap();
    assert!(ordinary.events().eq(iteration.interval().events()));
}

fn body_helper_probes(
    registry: &mut lash_kernel_doc::FunctionRegistry,
    ids: &std::collections::BTreeMap<&'static str, lash_kernel_value::names::FunctionId>,
) -> Vec<(&'static str, lash_kernel_value::names::FunctionId)> {
    use lash_kernel_doc::{NativeCall, NativeError, NativeFunction, PreparedNative, Value};
    struct Add;
    impl Add {
        fn answer(args: &[Value]) -> Option<Value> {
            use lash_kernel_doc::scalar::{self, ScalarOp};
            match args {
                [Value::Int(a), Value::Int(b)] => scalar::ints(ScalarOp::Add, *a, *b),
                [Value::Float(a), Value::Float(b)] => scalar::floats(ScalarOp::Add, *a, *b),
                _ => None,
            }
        }
    }
    impl NativeFunction for Add {
        fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
            Self::answer(call.args.values())
                .ok_or_else(|| NativeError::refused("scalar", "outside scalar domain"))
        }
        fn prepare(&self) -> Option<PreparedNative> {
            Some(PreparedNative {
                pair: None,
                charged_entry: None,
                entry: Self::answer,
                operation: "law.body_add",
            })
        }
        fn resource_contract(
            &self,
            args: lash_kernel_doc::Args<'_>,
        ) -> Result<Option<lash_kernel_doc::NativeResourceContract>, NativeError> {
            Self::answer(args.values())
                .map(|value| lash_kernel_doc::NativeResourceContract::returned(args, &value))
                .transpose()
        }
    }
    let mut functions = scalar_probes(registry, ids);
    let add = functions
        .iter()
        .find(|(name, _)| *name == "flat.add")
        .unwrap()
        .1;
    let lt = functions
        .iter()
        .find(|(name, _)| *name == "flat.lt")
        .unwrap()
        .1;
    for (name, body) in [
        ("body.plain", "return flat.add(a,b)"),
        (
            "body.guarded",
            "if flat.lt(a,b) { return flat.add(a,b) } return flat.add(a,b)",
        ),
    ] {
        let definition = lash_kernel_doc::parse_definition(&format!("function {name}(a: Any, b: Any) -> Any\nkernel 1\ncharge sum(7, size(a), size(b), size(result))\nuse flat.add = @{add}\nuse flat.lt = @{lt}\nnative\nbody {{ {body} }}")).unwrap();
        functions.push((
            name,
            registry
                .register(definition, Some(std::sync::Arc::new(Add)))
                .unwrap(),
        ));
    }
    let implementation = std::sync::Arc::clone(registry.get(&lt).unwrap().native().unwrap());
    let definition = lash_kernel_doc::parse_definition(&format!("function body.lt(a: Any, b: Any) -> Bool\nkernel 1\ncharge sum(7, size(a), size(b), size(result))\nuse flat.lt = @{lt}\nnative\nbody {{ return flat.lt(a,b) }}")).unwrap();
    functions.push((
        "body.lt",
        registry.register(definition, Some(implementation)).unwrap(),
    ));
    let identity = functions
        .iter()
        .find(|(name, _)| *name == "flat.identity")
        .unwrap()
        .1;
    let implementation = std::sync::Arc::clone(registry.get(&identity).unwrap().native().unwrap());
    let inner = lash_kernel_doc::parse_definition(&format!("function body.identity(value: Any) -> Any\nkernel 1\ncharge sum(7, size(value), size(result))\nuse flat.identity = @{identity}\nnative\nbody {{ return flat.identity(value) }}")).unwrap();
    let inner = registry
        .register(inner, Some(std::sync::Arc::clone(&implementation)))
        .unwrap();
    functions.push(("body.identity", inner));
    let chain = lash_kernel_doc::parse_definition(&format!("function body.chain(value: Any) -> Any\nkernel 1\ncharge sum(7, size(value), size(result))\nuse body.identity = @{inner}\nnative\nbody {{ let output = invoke body.identity(value) return output }}")).unwrap();
    functions.push((
        "body.chain",
        registry.register(chain, Some(implementation)).unwrap(),
    ));
    let charged = lash_kernel_doc::parse_definition(&format!("function body.charged_identity(value: Any) -> Any\nkernel 1\nmetering statements\nuse flat.identity = @{identity}\nbody {{ return flat.identity(value) }}")).unwrap();
    let charged = registry.register(charged, None).unwrap();
    let charged_chain = lash_kernel_doc::parse_definition(&format!("function body.charged_chain(value: Any) -> Any\nkernel 1\nmetering statements\nuse flat.identity = @{identity}\nuse flat.add = @{add}\nuse body.charged_identity = @{charged}\nbody {{ let saved = flat.identity(value) let output = invoke body.charged_identity(value) return flat.add(saved, output) }}")).unwrap();
    functions.push(("body.charged_identity", charged));
    functions.push((
        "body.charged_chain",
        registry.register(charged_chain, None).unwrap(),
    ));
    let identity_native = std::sync::Arc::clone(registry.get(&identity).unwrap().native().unwrap());
    let (guarded, locals) = lash_kernel_doc::parse_definition_with_locals(&format!("function body.guarded_chain(value: Any) -> Any\nkernel 1\ncharge sum(7, size(value), size(result))\nuse flat.lt = @{lt}\nuse flat.identity = @{identity}\nnative\nbody {{ if flat.lt(value, 1) {{ return value }} let latent = 0 return fn() {{ set latent = flat.identity(latent) return latent }} }}")).unwrap();
    for local in locals {
        registry.register(local, None).unwrap();
    }
    functions.push((
        "body.guarded_chain",
        registry.register(guarded, Some(identity_native)).unwrap(),
    ));
    functions
}

fn scope_frontier(
    exe: &crate::compile::Executable,
    code: crate::compile::CodeId,
    call: &crate::compile::room::region::calls::VerifiedCall,
    ledger: &lash_kernel_state::kernel_internals::logical::Ledger,
    heap: &lash_kernel_state::kernel_internals::PagedHeap,
    invocation: &lash_kernel_state::kernel_internals::logical::Invocation,
    link: lash_kernel_value::LinkId,
) -> crate::compile::room::publication::Frontier {
    exe.code(code)
        .source_room
        .frontier(
            ledger,
            heap,
            invocation,
            link,
            lash_kernel_doc::KernelVersion::One,
            crate::compile::room::publication::Position {
                at: call.at().clone(),
                unpaid: Some(call.at().clone()),
                recovery: [0; 32],
                occurrence: crate::compile::room::publication::Occurrence {
                    task: lash_kernel_doc::TaskId::MAIN,
                    invocation: invocation.id(),
                    loops: vec![],
                    action: None,
                },
            },
        )
        .unwrap()
}
