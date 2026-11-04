//! Actual dynamic callable identity and saved graph content remain source facts.
use super::*;
use crate::compile::{
    CodeId,
    room::{self, placement::BindingPlacement, region::calls::VerifiedCall},
};
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_state::{
    arena::Arena,
    kernel_internals::logical::{self, RootValue},
};
use std::sync::Arc;

#[path = "callback/initial.rs"]
mod initial;

pub(super) fn identities() {
    initial::payment();
    production_callbacks();
    for layout in [0, 42] {
        for (rooted, foreign_binding, earlier, captured, mutates, two) in [
            (false, false, false, false, false, false),
            (true, false, false, false, false, false),
            (false, true, false, false, false, false),
            (true, true, false, false, false, false),
            (false, false, true, false, false, false),
            (false, false, false, true, false, false),
            (true, false, false, true, false, false),
            (false, false, false, true, true, false),
            (false, false, false, true, false, true),
            (true, false, false, true, false, true),
        ] {
            let reversed =
                lash_kernel_value::Name::new("seed") < lash_kernel_value::Name::new("offset");
            let expression = if earlier {
                "(apply f(1, 2), apply f(3, 4))"
            } else if rooted {
                "apply f(3, 4)"
            } else {
                "(outer, apply f(3, 4))"
            };
            let definition = crate::laws::embedder::fragment(&format!(
                "function callback(f: Any, outer: Any) -> Any\nkernel 1\nmetering statements\nbody {{ let answer = {expression} return answer }}"
            ));
            let make_body = if two {
                if reversed {
                    "return fn(a, b) { return flat.rem(offset, seed) }"
                } else {
                    "return fn(a, b) { return flat.rem(seed, offset) }"
                }
            } else if mutates {
                "return fn(a, b) { set seed = flat.rem(a, b) return seed }"
            } else {
                "return fn(a, b) { return flat.rem(a, seed) }"
            };
            let make_params = if two {
                "seed: Any, offset: Any"
            } else {
                "seed: Any"
            };
            let maker = crate::laws::embedder::fragment(&format!(
                "function make({make_params}) -> Any\nkernel 1\nmetering statements\nbody {{ {make_body} }}"
            ));
            let first = if two {
                "call make(9, 7)"
            } else if captured {
                "call make(9)"
            } else {
                "fn(a, b) { return flat.rem(a, b) }"
            };
            let text = format!(
                "definition \"{definition}\"\ndefinition \"{maker}\"\nmain {{ let first = {first} let second = fn(a, b) {{ return flat.rem(b, a) }} let answer = call callback(first, []) finish answer }}"
            );
            let run = Embedder::with(
                &text,
                Setup {
                    layout: crate::Layout(layout),
                    probes: Some(crate::laws::layout::scalar_probes),
                    ..Setup::default()
                },
            );
            let (program, _) = run.program(true);
            let admitted = program
                .library
                .admit(&program.document, Default::default(), &program.selection)
                .unwrap_or_else(|_| panic!("callback source admitted"));
            let exe = admitted
                .executable(
                    &program.document,
                    &program.library,
                    &program.documents,
                    &program.functions,
                    &program.instances,
                )
                .unwrap_or_else(|_| panic!("callback executable"));
            let caller = exe
                .islands
                .plans
                .iter()
                .find(|plan| {
                    let code = exe.code(plan.code);
                    code.created.is_none()
                        && code.source_room.params.as_ref()
                            == [
                                lash_kernel_value::Name::new("f"),
                                lash_kernel_value::Name::new("outer"),
                            ]
                        && code.source_room.body.len() == 2
                })
                .unwrap();
            let statement = caller.body.statements[0].id;
            let (stream, stmt, _) = exe.statement(statement);
            let pc = (stmt.ops.start..stmt.ops.end())
                .rfind(|pc| {
                    matches!(
                        stream.ops[*pc as usize],
                        lash_kernel_exec::Op::Act | lash_kernel_exec::Op::Nested { .. }
                    )
                })
                .unwrap();
            let source = &exe.code(caller.code).source_room;
            let code_at = |ordinal| {
                (0..exe.code_count())
                    .map(|index| CodeId(index as u32))
                    .find(|id| {
                        exe.code(*id).created.as_ref().is_some_and(|at| {
                            at.unit == lash_kernel_value::sites::Unit::Main
                                && at.path.first() == Some(&ordinal)
                        })
                    })
                    .unwrap()
            };
            let callee = if captured {
                (0..exe.code_count())
                    .map(|index| CodeId(index as u32))
                    .find(|id| {
                        exe.code(*id).created.is_some()
                            && exe.code(*id).capture_names.len() == if two { 2 } else { 1 }
                            && exe
                                .code(*id)
                                .capture_names
                                .contains(&lash_kernel_value::Name::new("seed"))
                            && exe.code(*id).source_room.params.as_ref()
                                == [
                                    lash_kernel_value::Name::new("a"),
                                    lash_kernel_value::Name::new("b"),
                                ]
                    })
                    .unwrap()
            } else {
                code_at(0)
            };
            let canonical = |candidate| {
                let code = exe.code(candidate);
                let at = code.created.as_ref().unwrap();
                let origin = lash_kernel_doc::snapshot::closure_at(
                    &program.document,
                    at,
                    program.link,
                    exe.helpers(),
                    exe.selection(),
                )
                .unwrap();
                crate::machine::project::closure_code(
                    &exe,
                    &lash_kernel_value::CallableId {
                        link: program.link,
                        function: origin.function,
                    },
                    at,
                    &code.capture_names,
                )
                .unwrap()
            };
            let callee = canonical(callee);
            let other = canonical(code_at(1));
            assert_ne!(callee, other);
            assert!(
                VerifiedCall::compile_from(&exe, caller.code, statement, pc, stmt.ops.start)
                    .is_err(),
                "an unresolved value call cannot fabricate a target"
            );
            if earlier {
                assert!(
                    VerifiedCall::compile_callback(
                        &exe,
                        caller.code,
                        statement,
                        pc,
                        stmt.ops.start,
                        callee
                    )
                    .is_err(),
                    "one composed debt cannot silently omit an earlier entered guest body"
                );
                continue;
            }
            if mutates {
                assert!(
                    VerifiedCall::compile_callback(
                        &exe,
                        caller.code,
                        statement,
                        pc,
                        stmt.ops.start,
                        callee
                    )
                    .is_err(),
                    "a body that mutates its shared capture remains an exact handoff"
                );
                continue;
            }
            let call = Arc::new(
                VerifiedCall::compile_callback(
                    &exe,
                    caller.code,
                    statement,
                    pc,
                    stmt.ops.start,
                    callee,
                )
                .unwrap(),
            );
            let alternate = VerifiedCall::compile_callback(
                &exe,
                caller.code,
                statement,
                pc,
                stmt.ops.start,
                other,
            )
            .unwrap();
            assert_eq!(call.verify(&exe), Ok(()));
            let mut arena = Arena::new();
            let code = exe.code(callee);
            let at = code.created.as_ref().unwrap();
            let origin = lash_kernel_doc::snapshot::closure_at(
                &program.document,
                at,
                program.link,
                exe.helpers(),
                exe.selection(),
            )
            .unwrap();
            let identity = |link| {
                Arc::new(lash_kernel_doc::arena::ClosureIdentity {
                    capture_names: code.capture_names.clone(),
                    callable: lash_kernel_value::CallableId {
                        link,
                        function: origin.function,
                    },
                    site: lash_kernel_doc::linked_site::LinkedSite {
                        link,
                        site: at.clone(),
                    },
                })
            };
            let defining_link = if foreign_binding {
                lash_kernel_value::LinkId::from_bytes([99; 32])
            } else {
                program.link
            };
            let cell = captured.then(|| arena.new_cell(Value::Int(9)));
            let offset = two.then(|| arena.new_cell(Value::Int(7)));
            let cells: Vec<_> = if let Some(offset) = offset {
                let seed = cell.unwrap();
                let names: Vec<_> = code
                    .captures
                    .iter()
                    .map(|capture| code.slots[capture.inner as usize].name.clone())
                    .collect();
                assert_ne!(
                    names, code.capture_names,
                    "the fixture must distinguish cell order from the sorted identity set"
                );
                if reversed {
                    vec![offset, seed]
                } else {
                    vec![seed, offset]
                }
            } else {
                cell.into_iter().collect()
            };
            let actual = Value::Closure(arena.new_closure(identity(defining_link), cells.clone()));
            let foreign = Value::Closure(arena.new_closure(
                identity(lash_kernel_value::LinkId::from_bytes([99; 32])),
                cells.clone(),
            ));
            let substituted =
                Value::Closure(arena.new_closure(identity(program.link), cells.clone()));
            let outer = arena.create_list(&[Value::Int(9)]);
            let Value::List(handle) = outer else {
                panic!("saved graph");
            };
            let heap = run.machine.source_law_heap();
            let store = heap.logical_ledger();
            let mut ledger = store.borrow_mut();
            let root_slot = |name: &str| {
                let root = source
                    .owner_slots()
                    .unwrap()
                    .root(
                        &room::Owner::Binding {
                            declared: source.site.clone(),
                            name: lash_kernel_value::Name::new(name),
                        },
                        TaskId::MAIN,
                        0,
                    )
                    .unwrap();
                let logical::Owner::Value { slot, .. } = root else {
                    panic!("parameter role");
                };
                slot
            };
            let f_slot = root_slot("f");
            let outer_slot = root_slot("outer");
            let roles: Vec<_> = call
                .scope_roles()
                .enumerate()
                .map(|(index, slot)| {
                    let value = if index < 2 {
                        Value::Int(3 + index as i64)
                    } else if rooted || index == 3 {
                        actual
                    } else {
                        outer
                    };
                    (slot, value)
                })
                .collect();
            let entry_roots = [
                (f_slot, RootValue::Value(&actual)),
                (outer_slot, RootValue::Value(&outer)),
            ];

            let (invocation, prepared) = ledger
                .prepare_enter(TaskId::MAIN, &mut arena, heap.pages(), &[], &entry_roots)
                .unwrap();
            ledger
                .commit(prepared.admit(u64::MAX).ok().unwrap())
                .unwrap();
            let registration = arena.registration().unwrap();
            let operands = roles
                .iter()
                .filter(|(slot, _)| {
                    ledger
                        .holding(invocation.owner(*slot), heap.pages())
                        .unwrap()
                        .is_none()
                })
                .map(|(slot, value)| (invocation.owner(*slot), RootValue::Value(value)))
                .collect::<Vec<_>>();
            ledger
                .prepare_source_boundary(registration, heap, &[], &operands)
                .unwrap()
                .admit(u64::MAX)
                .ok()
                .unwrap()
                .publish(&mut ledger, |_, edits| assert!(edits.is_empty()))
                .unwrap();

            let placement = BindingPlacement::compile(&exe, caller.code).unwrap();
            let mut physical =
                vec![crate::machine::SlotState::Empty; exe.code(caller.code).slots.len()];
            physical[placement.physical(f_slot).unwrap() as usize] =
                crate::machine::SlotState::Value(actual);
            physical[placement.physical(outer_slot).unwrap() as usize] =
                crate::machine::SlotState::Value(outer);
            let activation = placement
                .capture(caller.code, &invocation, &ledger, heap, &physical)
                .unwrap()
                .unwrap();
            drop(ledger);
            let frontier = call_caller_frontier(
                &exe,
                &call,
                &store.borrow(),
                heap,
                &invocation,
                program.link,
                room::publication::Position {
                    at: call.at().clone(),
                    unpaid: Some(call.at().clone()),
                    recovery: [0; 32],
                    occurrence: room::publication::Occurrence {
                        task: TaskId::MAIN,
                        invocation: invocation.id(),
                        loops: vec![],
                        action: None,
                    },
                },
            );
            let ledger = store.borrow();
            let initial = ledger.prospective_invocation(TaskId::MAIN).id();
            if foreign_binding {
                assert!(
                    call.bind_callback_scope(
                        &exe,
                        frontier.call_entry().unwrap(),
                        &ledger,
                        &arena,
                        &invocation,
                        &activation,
                        ledger.reserve_invocations(TaskId::MAIN, 2).unwrap(),
                        actual
                    )
                    .is_none(),
                    "an actual source binding still needs an admitted defining Link"
                );
                assert_eq!(ledger.prospective_invocation(TaskId::MAIN).id(), initial);
                continue;
            }
            assert!(
                call.bind_scope(
                    &exe,
                    frontier.call_entry().unwrap(),
                    callee,
                    &ledger,
                    &invocation,
                    &activation,
                    ledger.reserve_invocations(TaskId::MAIN, 2).unwrap()
                )
                .is_none(),
                "a dynamic scope cannot bypass actual callable authentication"
            );
            assert!(
                call.bind_callback_scope(
                    &exe,
                    frontier.call_entry().unwrap(),
                    &ledger,
                    &arena,
                    &invocation,
                    &activation,
                    ledger.reserve_invocations(TaskId::MAIN, 2).unwrap(),
                    foreign
                )
                .is_none(),
                "a foreign defining Link cannot enter the scope"
            );
            assert!(
                alternate
                    .bind_callback_scope(
                        &exe,
                        frontier.call_entry().unwrap(),
                        &ledger,
                        &arena,
                        &invocation,
                        &activation,
                        ledger.reserve_invocations(TaskId::MAIN, 2).unwrap(),
                        actual
                    )
                    .is_none(),
                "a same-arity source cannot replace the callable at scope entry"
            );
            assert!(
                call.bind_callback_scope(
                    &exe,
                    frontier.call_entry().unwrap(),
                    &ledger,
                    &arena,
                    &invocation,
                    &activation,
                    ledger.reserve_invocations(TaskId::MAIN, 2).unwrap(),
                    substituted
                )
                .is_none(),
                "another closure with the same code cannot replace the actual source binding"
            );
            let range = ledger.reserve_invocations(TaskId::MAIN, 2).unwrap();
            let scope = call
                .bind_callback_scope(
                    &exe,
                    frontier.call_entry().unwrap(),
                    &ledger,
                    &arena,
                    &invocation,
                    &activation,
                    range,
                    actual,
                )
                .unwrap_or_else(|| {
                    panic!(
                        "callback scope miss layout={layout} rooted={rooted} captured={captured}"
                    )
                });
            assert_eq!(
                roles.len(),
                if rooted { 3 } else { 4 },
                "ordered arguments, saved outer operand and actual value callee"
            );
            assert!(
                alternate
                    .evaluate_scoped(&exe, other, &scope, 0, &arena, &roles)
                    .unwrap()
                    .is_none(),
                "a same-arity alternative cannot substitute for the actual callable"
            );
            let mut foreign_roles = roles.clone();
            foreign_roles.last_mut().unwrap().1 = foreign;
            assert!(
                call.evaluate_scoped(&exe, callee, &scope, 0, &arena, &foreign_roles)
                    .unwrap()
                    .is_none(),
                "the same function under a foreign Link is not admitted"
            );
            let candidate = call
                .evaluate_scoped(&exe, callee, &scope, 0, &arena, &roles)
                .unwrap()
                .unwrap();
            let expected = if two { if reversed { 7 } else { 2 } } else { 3 };
            assert!(
                candidate.result().same(&Value::Int(expected)),
                "the actual ordered capture values, rather than their equal prices, determine the answer"
            );
            assert_eq!(
                candidate.peak(),
                lash_kernel_doc::LogicalHeldV1::FRAME
                    + 3 * (lash_kernel_doc::LogicalHeldV1::REFERENCE + 1),
                "borrowed cell roots add no holding price; both reads and the native result coexist with the real frame"
            );
            assert_eq!(candidate.invocation().id(), initial);
            let room = candidate.room();
            assert!(
                room.checked(
                    &ledger,
                    &arena,
                    source,
                    call.at(),
                    &invocation,
                    candidate.invocation()
                )
                .is_some()
            );
            let after = room::Point {
                site: call.source().site.clone(),
                phase: room::Phase::Leave,
            };
            let recovery = candidate
                .recovery_after(
                    &after,
                    room::publication::Occurrence {
                        task: TaskId::MAIN,
                        invocation: invocation.id(),
                        loops: vec![],
                        action: None,
                    },
                )
                .unwrap();
            assert!(Arc::ptr_eq(&recovery[0].source, source));
            assert!(Arc::ptr_eq(&recovery[1].source, call.source()));
            assert!(
                recovery[1].captures.is_empty(),
                "defining Leave releases every callee capture root"
            );
            if let Some(cell) = cell {
                let read = room::Point {
                    site: call.source().site.child(0).child(0).child(1),
                    phase: room::Phase::Commit,
                };
                let during = candidate
                    .recovery_after(
                        &read,
                        room::publication::Occurrence {
                            task: TaskId::MAIN,
                            invocation: invocation.id(),
                            loops: vec![],
                            action: None,
                        },
                    )
                    .unwrap();
                assert_eq!(during[1].captures.len(), if two { 2 } else { 1 });
                assert_eq!(
                    during[1]
                        .captures
                        .iter()
                        .map(|(_, actual)| *actual)
                        .collect::<Vec<_>>(),
                    cells,
                    "cold capture roles preserve the actual environment order"
                );
                assert!(
                    during[1].captures.iter().any(|(_, actual)| *actual == cell),
                    "cold callee recovery retains the actual shared cell"
                );
                arena.cell_set(cell, Value::Int(10)).unwrap();
                assert!(
                    room.checked(
                        &ledger,
                        &arena,
                        source,
                        call.at(),
                        &invocation,
                        candidate.invocation()
                    )
                    .is_none(),
                    "a same-price captured-cell replacement expires the composed body evidence"
                );
            }
            if !rooted {
                assert!(
                    recovery[0]
                        .roles
                        .iter()
                        .any(|(_, value)| value.same(&outer)),
                    "the exact saved graph operand survives the callee"
                );
                arena.list_set(handle, 0, Value::Int(10)).unwrap();
                assert!(
                    room.checked(
                        &ledger,
                        &arena,
                        source,
                        call.at(),
                        &invocation,
                        candidate.invocation()
                    )
                    .is_none(),
                    "unchanged table position cannot authenticate replaced saved content"
                );
            }
            drop(candidate);
            assert_eq!(scope.completed(), 0);
            drop(scope);
            assert_eq!(
                ledger.prospective_invocation(TaskId::MAIN).id(),
                initial,
                "abandoned callback candidates spend no invocation"
            );
            if let Some(cell) = cell {
                arena.cell_set(cell, Value::Int(9)).unwrap();
            }
            if !rooted {
                arena.list_set(handle, 0, Value::Int(9)).unwrap();
            }
            drop(ledger);
            affine_callback(
                &exe,
                &call,
                &activation,
                &store,
                &mut arena,
                heap,
                &invocation,
                &roles,
                actual,
                expected,
                program.link,
                captured,
            );
        }
    }
}

/// Scoped completion must settle once with real roots, original debt and IDs.
#[expect(
    clippy::too_many_arguments,
    reason = "the existing callback fixture's admitted entry"
)]
fn affine_callback(
    exe: &crate::compile::Executable,
    call: &Arc<VerifiedCall>,
    activation: &room::placement::DenseActivation<'_>,
    store: &std::cell::RefCell<logical::Ledger>,
    arena: &mut Arena,
    heap: &lash_kernel_state::kernel_internals::PagedHeap,
    caller: &logical::Invocation,
    roles: &[(logical::Slot, Value)],
    actual: Value,
    expected: i64,
    link: lash_kernel_value::LinkId,
    captured: bool,
) {
    use room::publication::Occurrence;
    let plan = call.publication_plan_for_law(exe).unwrap();
    let occurrence = Occurrence {
        task: TaskId::MAIN,
        invocation: caller.id(),
        loops: vec![],
        action: None,
    };
    let mut start = plan.start(occurrence.clone());
    start.at = call.at().clone();
    let initial_ledger = store.borrow();
    let mut frontier = call_caller_frontier(
        exe,
        call,
        &initial_ledger,
        heap,
        caller,
        link,
        start.clone(),
    );
    let foreign_frontier = call_caller_frontier(
        exe,
        call,
        &initial_ledger,
        heap,
        caller,
        link,
        start.clone(),
    );
    drop(initial_ledger);
    let mut locked = store.borrow_mut();
    let ledger = &mut *locked;
    let next = plan.exit(occurrence.clone());
    let first_id = ledger.prospective_invocation(TaskId::MAIN).id();
    let before = ledger.balance().unwrap();
    let before_total = before.total().unwrap();
    let output = call_caller_source(exe, call)
        .owner_slots()
        .unwrap()
        .root(
            &room::Owner::Operand(call.at().site.clone()),
            TaskId::MAIN,
            caller.id(),
        )
        .unwrap();
    let inputs: Vec<_> = call
        .consumed_roles()
        .map(|slot| caller.owner(slot))
        .collect();
    assert_eq!(
        inputs.len(),
        2 + usize::from(
            roles
                .last()
                .is_some_and(|(slot, _)| matches!(slot, logical::Slot::Operand(_)))
        )
    );
    {
        let (scope, candidate) = evaluate_callback(
            exe, &frontier, call, activation, caller, roles, actual, arena, ledger,
        );
        let prefix = scope
            .consume_prefix(arena, vec![candidate])
            .unwrap()
            .unwrap();
        assert!(
            prefix
                .bind_call_for_law(&plan, &foreign_frontier, ledger, arena, heap, next.clone())
                .unwrap()
                .is_none(),
            "an equal-position foreign Frontier cannot substitute the actual entry"
        );
    }
    let mut charged = 0;
    let count = call_scope_occurrences(call, exe);
    let mut counters = crate::machine::tasks::Occurrences::for_source_law(count);
    for (memory, budget, wrong_output) in [
        (0, u64::MAX, false),
        (u64::MAX, 0, false),
        (u64::MAX, u64::MAX, true),
        (u64::MAX, u64::MAX, false),
    ] {
        let (scope, candidate) = evaluate_callback(
            exe, &frontier, call, activation, caller, roles, actual, arena, ledger,
        );
        let memory = if memory == 0 {
            before_total + candidate.peak() - 1
        } else {
            memory
        };
        let prefix = scope
            .consume_prefix(arena, vec![candidate])
            .unwrap()
            .unwrap();
        assert_eq!(prefix.completed_invocations(), 1);
        assert!(prefix.calls()[0].result().same(&Value::Int(expected)));
        let leave = room::Point {
            site: call.source().site.clone(),
            phase: room::Phase::Leave,
        };
        let cold = prefix.calls()[0]
            .recovery_after(&leave, occurrence.clone())
            .unwrap();
        assert!(Arc::ptr_eq(&cold[1].source, call.source()));
        assert_eq!(cold[1].position.occurrence.invocation, first_id);
        if captured {
            let read = room::Point {
                site: call.source().site.child(0).child(0).child(1),
                phase: room::Phase::Commit,
            };
            assert!(
                !prefix.calls()[0]
                    .recovery_after(&read, occurrence.clone())
                    .unwrap()[1]
                    .captures
                    .is_empty()
            );
        }
        let proof = prefix
            .bind_call_for_law(&plan, &frontier, ledger, arena, heap, next.clone())
            .unwrap()
            .unwrap();
        let result = Value::Int(if wrong_output { expected + 1 } else { expected });
        let registration = arena.registration().unwrap();
        let transaction = ledger
            .prepare_source_boundary(
                registration,
                heap,
                &inputs,
                &[(output, RootValue::Value(&result))],
            )
            .unwrap();
        let resources = crate::admission::SourceResources {
            charged: &mut charged,
            counters: &mut counters,
            bound: budget,
            slice: u64::MAX,
            depth: 1,
            limit: u64::MAX,
            safe_point: 0,
        };
        let accepted = proof
            .prepare_external(&frontier, ledger, heap, transaction, resources, memory)
            .unwrap();
        if memory == u64::MAX && budget == u64::MAX && !wrong_output {
            assert!(
                accepted.is_some(),
                "roomy prefix wins provisional admission"
            );
            drop(accepted);
        } else {
            assert!(accepted.is_none());
        }
        assert_eq!(charged, 0);
        assert_eq!(ledger.balance().unwrap(), before);
        assert_eq!(ledger.prospective_invocation(TaskId::MAIN).id(), first_id);
        assert_eq!(frontier.position(), &start);
    }
    {
        let (scope, first) = evaluate_callback(
            exe, &frontier, call, activation, caller, roles, actual, arena, ledger,
        );
        let second = call
            .evaluate_scoped(exe, call.callee(), &scope, 1, arena, roles)
            .unwrap()
            .unwrap();
        let prefix = scope
            .consume_prefix(arena, vec![first, second])
            .unwrap()
            .unwrap();
        assert_eq!(prefix.completed_invocations(), 2);
        assert_eq!(prefix.calls()[0].invocation().id(), first_id);
        assert_eq!(prefix.calls()[1].invocation().id(), first_id + 1);
        drop(prefix);
        assert_eq!(ledger.prospective_invocation(TaskId::MAIN).id(), first_id);
    }
    let (foreign_scope, foreign_candidate) = evaluate_callback(
        exe, &frontier, call, activation, caller, roles, actual, arena, ledger,
    );
    let (scope, candidate) = evaluate_callback(
        exe, &frontier, call, activation, caller, roles, actual, arena, ledger,
    );
    drop(candidate);
    assert!(
        scope
            .consume_prefix(arena, vec![foreign_candidate])
            .unwrap()
            .is_none(),
        "foreign scope cannot supply a candidate"
    );
    drop(foreign_scope);
    if captured {
        let (scope, candidate) = evaluate_callback(
            exe, &frontier, call, activation, caller, roles, actual, arena, ledger,
        );
        let prefix = scope
            .consume_prefix(arena, vec![candidate])
            .unwrap()
            .unwrap();
        let read = room::Point {
            site: call.source().site.child(0).child(0).child(1),
            phase: room::Phase::Commit,
        };
        let cell = prefix.calls()[0]
            .recovery_after(&read, occurrence.clone())
            .unwrap()[1]
            .captures[0]
            .1;
        let old = arena.cell(cell).unwrap();
        arena.cell_set(cell, Value::Int(10)).unwrap();
        assert!(
            prefix
                .bind_call_for_law(&plan, &frontier, ledger, arena, heap, next.clone())
                .unwrap()
                .is_none(),
            "sealed prefix retains actual captured-cell dependencies"
        );
        arena.cell_set(cell, old).unwrap();
    }
    if let Some(outer) = roles.iter().find_map(|(_, value)| match value {
        Value::List(handle) => Some(*handle),
        _ => None,
    }) {
        let (scope, candidate) = evaluate_callback(
            exe, &frontier, call, activation, caller, roles, actual, arena, ledger,
        );
        let prefix = scope
            .consume_prefix(arena, vec![candidate])
            .unwrap()
            .unwrap();
        arena.list_set(outer, 0, Value::Int(10)).unwrap();
        assert!(
            prefix
                .bind_call_for_law(&plan, &frontier, ledger, arena, heap, next.clone())
                .unwrap()
                .is_none(),
            "sealed prefix retains saved graph dependencies"
        );
        arena.list_set(outer, 0, Value::Int(9)).unwrap();
    }
    let (scope, candidate) = evaluate_callback(
        exe, &frontier, call, activation, caller, roles, actual, arena, ledger,
    );
    let debt = candidate.debt().unwrap();
    let expected_units = debt.units();
    let expected_tape = crate::ChargeTape::new();
    debt.record(&expected_tape);
    let prefix = scope
        .consume_prefix(arena, vec![candidate])
        .unwrap()
        .unwrap();
    drop(prefix);
    assert_eq!(ledger.prospective_invocation(TaskId::MAIN).id(), first_id);
    let (scope, candidate) = evaluate_callback(
        exe, &frontier, call, activation, caller, roles, actual, arena, ledger,
    );
    let peak = candidate.peak();
    let delta = candidate.delta()
        - if roles
            .last()
            .is_some_and(|(slot, _)| matches!(slot, logical::Slot::Operand(_)))
        {
            i128::from(arena.logical_local(&actual).unwrap())
        } else {
            0
        };
    let prefix = scope
        .consume_prefix(arena, vec![candidate])
        .unwrap()
        .unwrap();
    let proof = prefix
        .bind_call_for_law(&plan, &frontier, ledger, arena, heap, next.clone())
        .unwrap()
        .unwrap();
    let result = Value::Int(expected);
    let registration = arena.registration().unwrap();
    let transaction = ledger
        .prepare_source_boundary(
            registration,
            heap,
            &inputs,
            &[(output, RootValue::Value(&result))],
        )
        .unwrap();
    let resources = crate::admission::SourceResources {
        charged: &mut charged,
        counters: &mut counters,
        bound: u64::MAX,
        slice: u64::MAX,
        depth: 1,
        limit: u64::MAX,
        safe_point: 0,
    };
    let accepted = proof
        .prepare_external(
            &frontier,
            ledger,
            heap,
            transaction,
            resources,
            before_total + peak,
        )
        .unwrap()
        .unwrap();
    let applied = std::cell::Cell::new(0);
    let tape = crate::ChargeTape::new();
    let balance = accepted
        .publish_boundary(
            &mut frontier,
            ledger,
            |_, edits| {
                assert!(edits.is_empty());
                applied.set(applied.get() + 1);
            },
            Some(&tape),
        )
        .unwrap();
    assert_eq!(applied.get(), 1);
    assert_eq!(frontier.position(), &next);
    assert_eq!(charged, expected_units);
    assert_eq!(tape.steps(), expected_tape.steps());
    assert_eq!(
        ledger.prospective_invocation(TaskId::MAIN).id(),
        first_id + 1
    );
    assert_eq!(
        i128::from(balance.total().unwrap()) - i128::from(before_total),
        delta
    );
    for (slot, value) in roles
        .iter()
        .filter(|(slot, _)| !inputs.contains(&caller.owner(*slot)))
    {
        assert!(
            ledger
                .holding(caller.owner(*slot), heap.pages())
                .unwrap()
                .is_some(),
            "saved outer role survives"
        );
        assert!(value.handle().is_some());
    }
}

fn call_caller_source<'a>(
    exe: &'a crate::compile::Executable,
    call: &VerifiedCall,
) -> &'a Arc<room::Source> {
    (0..exe.code_count())
        .map(|i| CodeId(i as u32))
        .find_map(|id| {
            let source = &exe.code(id).source_room;
            call.belongs_to_for_law(source, id).then_some(source)
        })
        .unwrap()
}
fn call_caller_frontier(
    exe: &crate::compile::Executable,
    call: &VerifiedCall,
    ledger: &logical::Ledger,
    heap: &lash_kernel_state::kernel_internals::PagedHeap,
    caller: &logical::Invocation,
    link: lash_kernel_value::LinkId,
    start: room::publication::Position,
) -> room::publication::Frontier {
    call_caller_source(exe, call)
        .frontier(
            ledger,
            heap,
            caller,
            link,
            lash_kernel_doc::KernelVersion::One,
            start,
        )
        .unwrap()
}
fn call_scope_occurrences(call: &VerifiedCall, exe: &crate::compile::Executable) -> usize {
    let _ = exe;
    call.occurrences_for_law()
        .iter()
        .copied()
        .max()
        .map_or(0, |n| n + 1)
}

#[expect(
    clippy::too_many_arguments,
    reason = "existing fixture's authenticated callback entry"
)]
fn evaluate_callback<'ledger, 'plan>(
    exe: &crate::compile::Executable,
    frontier: &room::publication::Frontier,
    call: &'plan VerifiedCall,
    activation: &room::placement::DenseActivation<'_>,
    caller: &logical::Invocation,
    roles: &[(logical::Slot, Value)],
    actual: Value,
    arena: &Arena,
    ledger: &'ledger logical::Ledger,
) -> (
    room::region::calls::scope::CallScope<'ledger>,
    room::region::calls::scope::ScopedCall<'plan>,
) {
    let scope = call
        .bind_callback_scope(
            exe,
            frontier.call_entry().unwrap(),
            ledger,
            arena,
            caller,
            activation,
            ledger.reserve_invocations(TaskId::MAIN, 3).unwrap(),
            actual,
        )
        .unwrap();
    let candidate = call
        .evaluate_scoped(exe, call.callee(), &scope, 0, arena, roles)
        .unwrap()
        .unwrap();
    (scope, candidate)
}

/// K-EVAL-008: a production callback composes nested wrappers, rebinding after
/// capture mutation and retaining each repeated call's ordered source debt.
fn production_callbacks() {
    use crate::laws::layout::ExecutionMode;
    use crate::{Bounds, Machine, Step};
    let definition = |signature: &str, body: &str| {
        crate::laws::embedder::fragment(&format!(
            "function {signature} -> Any\nkernel 1\nmetering statements\nbody {{ {body} }}"
        ))
    };
    let definitions = format!(
        "definition \"{}\"\ndefinition \"{}\"\ndefinition \"{}\"\n",
        definition("leaf(a: Any, b: Any)", "return flat.add(a, b)"),
        definition(
            "middle(a: Any, b: Any)",
            "return flat.add(7, invoke leaf(a, b))"
        ),
        definition(
            "fold(f: Any, xs: Any)",
            "let total = 0 for x in xs { set total = apply f(total, x) } return total"
        )
    );
    let exercise = definition(
        "exercise()",
        "let seed = 3 let f = fn(a, b) { return flat.add(seed, invoke middle(a, b)) } let alias = f let xs = [1, 1, 2] let first = call fold(alias, xs) set seed = 9 let second = call fold(f, xs) return (first, second)",
    );
    let source =
        format!("{definitions}definition \"{exercise}\"\nmain {{ return call exercise() }}");
    let observe = |source: &str, mode, bounds: Bounds, slice, taped| {
        let mut run = Embedder::with(
            source,
            Setup {
                bounds,
                slice,
                probes: Some(crate::laws::layout::scalar_probes),
                ..Setup::default()
            },
        );
        let tape = crate::ChargeTape::new();
        if taped {
            run.machine.record_charges(tape.clone());
        }
        run.machine.take_callback_acceptances();
        let mut accepted = 0;
        let mut prefixes = Vec::new();
        let end = loop {
            match mode {
                ExecutionMode::Source => run.machine.force_source_execution(),
                ExecutionMode::Generic => run.machine.force_generic_packets(),
                ExecutionMode::Specialized => {}
            }
            let step = run.run();
            accepted += run.machine.take_callback_acceptances();
            let balance = run.machine.logical_balance().unwrap();
            prefixes.push((
                run.machine.meters(),
                [
                    balance.entity_local_bytes,
                    balance.root_and_control_bytes,
                    balance.entities,
                ],
                (!matches!(step, Step::Ended(_))).then(|| run.machine.semantic_checkpoint()),
            ));
            match step {
                Step::Ended(end) => break format!("{end:?}"),
                Step::Slice => {}
                Step::Parked(_) => {
                    run.deliver_next();
                }
            }
        };
        (
            (
                end,
                run.world.printed,
                run.requests,
                run.machine.meters().charged,
                tape.steps(),
                prefixes,
            ),
            accepted,
        )
    };
    let roomy = crate::laws::embedder::ROOMY;
    let header = crate::laws::embedder::fragment(
        "function walk(xs: Any, f: Any) -> Any\nkernel 1\ncharge 1\nbody { let out = [] for x in xs metered 2 { let y = apply f(x) set out[list.len(out)] = y } return out }",
    );
    let header_source = format!(
        "definition \"{header}\"\nmain {{ let xs = [1] let cb = fn(x) {{ return x }} return invoke walk(xs, cb) }}"
    );
    let header_total = observe(&header_source, ExecutionMode::Source, roomy, u64::MAX, true)
        .0
        .3
        .unwrap();
    for charge_limit in 0..=header_total {
        let bounds = Bounds {
            charge_limit,
            ..roomy
        };
        let exact = observe(
            &header_source,
            ExecutionMode::Source,
            bounds,
            u64::MAX,
            true,
        )
        .0;
        for mode in [ExecutionMode::Generic, ExecutionMode::Specialized] {
            assert_eq!(
                observe(&header_source, mode, bounds, u64::MAX, true).0,
                exact,
                "paid initial loop header at charge {charge_limit}, {mode:?}"
            );
        }
    }

    let (reference, _) = observe(&source, ExecutionMode::Source, roomy, u64::MAX, true);
    for mode in [ExecutionMode::Generic, ExecutionMode::Specialized] {
        let (actual, accepted) = observe(&source, mode, roomy, u64::MAX, true);
        assert_eq!(
            actual, reference,
            "nested callback source behavior {mode:?}"
        );
        if mode == ExecutionMode::Specialized {
            assert_eq!(
                accepted, 6,
                "each callback must use the production affine source publication"
            );
        }
    }
    // Find the source success edges, then compare the exact refusal and
    // success prefixes on both packet paths at those same bounds.
    let mut low = 0;
    let mut high = roomy.memory;
    while low + 1 < high {
        let middle = low + (high - low) / 2;
        let actual = observe(
            &source,
            ExecutionMode::Source,
            Bounds {
                memory: middle,
                ..roomy
            },
            u64::MAX,
            true,
        )
        .0;
        if actual.0 == reference.0 {
            high = middle;
        } else {
            low = middle;
        }
    }
    let exact_memory = high;
    let exact_depth = (1..=roomy.call_depth)
        .find(|depth| {
            observe(
                &source,
                ExecutionMode::Source,
                Bounds {
                    call_depth: *depth,
                    ..roomy
                },
                u64::MAX,
                true,
            )
            .0
            .0 == reference.0
        })
        .expect("source callback succeeds within its roomy depth");
    for bounds in [
        Bounds {
            memory: exact_memory - 1,
            ..roomy
        },
        Bounds {
            memory: exact_memory,
            ..roomy
        },
        Bounds {
            memory: exact_memory + 1,
            ..roomy
        },
        Bounds {
            call_depth: exact_depth - 1,
            ..roomy
        },
        Bounds {
            call_depth: exact_depth,
            ..roomy
        },
        Bounds {
            call_depth: exact_depth + 1,
            ..roomy
        },
    ] {
        let exact = observe(&source, ExecutionMode::Source, bounds, u64::MAX, true).0;
        for mode in [ExecutionMode::Generic, ExecutionMode::Specialized] {
            assert_eq!(
                observe(&source, mode, bounds, u64::MAX, true).0,
                exact,
                "callback exact cutoff {mode:?} {bounds:?}"
            );
        }
    }
    let exact_charge = reference.3.unwrap();
    for bounds in [
        Bounds {
            charge_limit: 0,
            ..roomy
        },
        Bounds {
            charge_limit: exact_charge - 1,
            ..roomy
        },
        Bounds {
            charge_limit: exact_charge,
            ..roomy
        },
    ] {
        let exact = observe(&source, ExecutionMode::Source, bounds, u64::MAX, true).0;
        for mode in [ExecutionMode::Generic, ExecutionMode::Specialized] {
            assert_eq!(
                observe(&source, mode, bounds, u64::MAX, true).0,
                exact,
                "callback cutoff {mode:?} {bounds:?}"
            );
        }
    }
    let exact = observe(&source, ExecutionMode::Source, roomy, 7, true).0;
    for mode in [ExecutionMode::Generic, ExecutionMode::Specialized] {
        assert_eq!(
            observe(&source, mode, roomy, 7, true).0,
            exact,
            "callback unpaid suffix {mode:?}"
        );
    }
    for body in [
        "if flat.lt(1, b) { throw \"stop\" } return flat.add(a,b)",
        "do perform 0(\"callback\") as Any return flat.add(a,b)",
        "set seed = flat.add(seed,1) return flat.add(a,b)",
    ] {
        let exercise = definition(
            "exercise()",
            &format!("let seed = 0 let f = fn(a,b) {{ {body} }} return call fold(f,[1,2,3])"),
        );
        let source =
            format!("{definitions}definition \"{exercise}\"\nmain {{ return call exercise() }}");
        let exact = observe(&source, ExecutionMode::Source, roomy, 7, true).0;
        for mode in [ExecutionMode::Generic, ExecutionMode::Specialized] {
            assert_eq!(
                observe(&source, mode, roomy, 7, true).0,
                exact,
                "opaque callback must hand off {body}"
            );
        }
    }
}
