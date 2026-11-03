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

pub(super) fn identities() {
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
            let call = VerifiedCall::compile_callback(
                &exe,
                caller.code,
                statement,
                pc,
                stmt.ops.start,
                callee,
            )
            .unwrap();
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
            let (invocation, prepared) = ledger
                .prepare_enter(
                    TaskId::MAIN,
                    &mut arena,
                    heap.pages(),
                    &[],
                    &[
                        (f_slot, RootValue::Value(&actual)),
                        (outer_slot, RootValue::Value(&outer)),
                    ],
                )
                .unwrap();
            ledger
                .commit(prepared.admit(u64::MAX).ok().unwrap())
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
            let initial = ledger.prospective_invocation(TaskId::MAIN).id();
            if foreign_binding {
                assert!(
                    call.bind_callback_scope(
                        &exe,
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
        }
    }
}
