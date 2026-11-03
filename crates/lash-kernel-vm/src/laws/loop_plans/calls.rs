//! K-EVAL-008 and K-BND-002: eliding a call cannot substitute its source,
//! omit the logical frame peak, lose saved operands or spend abandoned IDs.
use super::super::super::embedder::{Embedder, Setup};
use crate::compile::room::{
    ExitKind, Phase, Point, Refused, Selection,
    placement::BindingPlacement,
    publication::Occurrence,
    region::{ScalarRegionValues, calls::VerifiedCall},
};
use crate::machine::SlotState;
use lash_kernel_doc::{LogicalHeldV1 as Cost, TaskId, Value};
use lash_kernel_exec::Op;
use lash_kernel_state::{arena::Arena, kernel_internals::logical::Ledger};

pub(super) fn pure_calls() {
    entry_calls();
    guarded_calls();
    for layout in [0, 1, 42, u64::MAX] {
        let definition = |signature: &str, body: &str| {
            super::super::super::embedder::fragment(&format!(
                "function {signature} -> Any\nkernel 1\nmetering statements\nbody {{ {body} }}"
            ))
        };
        let text = format!(
            "definition \"{}\"\ndefinition \"{}\"\ndefinition \"{}\"\nmain {{ let alternate = invoke other(4, 3) let answer = invoke caller() finish answer }}",
            definition(
                "multiply(a: Any, b: Any)",
                "let product = flat.rem(a, b) return product"
            ),
            definition(
                "other(a: Any, b: Any)",
                "let product = flat.rem(b, a) return product"
            ),
            definition(
                "caller()",
                "let answer = flat.add(5, invoke multiply(3, 4)) return answer"
            )
        );
        let run = Embedder::with(
            &text,
            Setup {
                layout: crate::Layout(layout),
                probes: Some(super::super::super::layout::scalar_probes),
                ..Setup::default()
            },
        );
        let (program, _) = run.program(true);
        let admitted = program
            .library
            .admit(&program.document, Default::default(), &program.selection)
            .unwrap_or_else(|_| panic!("pure call source admitted"));
        let exe = admitted
            .executable(
                &program.document,
                &program.library,
                &program.documents,
                &program.functions,
                &program.instances,
            )
            .unwrap_or_else(|_| panic!("pure call executable"));
        let caller = exe
            .islands
            .plans
            .iter()
            .find(|plan| {
                let source = &exe.code(plan.code).source_room;
                source.params.is_empty()
                    && source.body.len() == 2
                    && matches!(source.body[0], lash_kernel_doc::Stmt::Let { .. })
            })
            .unwrap();
        let statement = caller.body.statements[0].id;
        let (stream, stmt, _) = exe.statement(statement);
        let pc = (stmt.ops.start..stmt.ops.end())
            .find(|pc| matches!(stream.ops[*pc as usize], Op::Nested { .. }))
            .unwrap();
        let mut call = VerifiedCall::compile(&exe, caller.code, statement, pc).unwrap();
        assert_eq!(call.verify(&exe), Ok(()));
        let source = &exe.code(caller.code).source_room;
        let points: Vec<_> = (stmt.ops.start..pc)
            .filter(|pc| matches!(stream.ops[*pc as usize], Op::Const { .. }))
            .map(|pc| Point {
                site: (**stream.source_sites[pc as usize].as_ref().unwrap()).clone(),
                phase: Phase::Commit,
            })
            .collect();
        assert_eq!(
            points.len(),
            3,
            "the saved left operand and two ordered arguments are evaluated before binding"
        );
        let route: Vec<_> = points
            .iter()
            .map(|at| Selection::Event {
                at: at.clone(),
                exit: ExitKind::Normal,
            })
            .collect();
        let interval = source
            .compile_between(&points[0], points.last().unwrap(), &route, route.len())
            .unwrap();
        let region = source.compile_scalar_region(interval).unwrap();
        let mut values = vec![Value::Null; region.fact_count()];
        for (at, fact) in region.results() {
            values[fact.index()] = if *at == points[0] {
                Value::Int(5)
            } else if *at == points[1] {
                Value::Int(3)
            } else {
                Value::Int(4)
            };
        }
        let mut ledger = Ledger::fresh();
        let mut arena = Arena::new();
        let heap = run.machine.source_law_heap();
        let (invocation, entered) = ledger
            .prepare_enter(TaskId::MAIN, &mut arena, heap.pages(), &[], &[])
            .unwrap();
        ledger
            .commit(entered.admit(u64::MAX).ok().unwrap())
            .unwrap();
        let placement = BindingPlacement::compile(&exe, caller.code).unwrap();
        let physical: Vec<_> = (0..exe.code(caller.code).slots.len())
            .map(|_| SlotState::Empty)
            .collect();
        let activation = placement
            .capture(caller.code, &invocation, &ledger, heap, &physical)
            .unwrap()
            .unwrap();
        let bound_caller = region
            .bind_activation(
                &ledger,
                &mut arena,
                heap,
                &invocation,
                &activation,
                ScalarRegionValues {
                    values: &values,
                    calls: &[],
                },
            )
            .unwrap()
            .unwrap();
        // A broader source prefix cannot borrow this statement's smaller
        // charge debt, even when its final operands and values are identical.
        let entered_at = Point {
            site: source.site.clone(),
            phase: Phase::Enter,
        };
        let broader_route: Vec<_> = std::iter::once(Selection::Event {
            at: entered_at.clone(),
            exit: ExitKind::Normal,
        })
        .chain(route.iter().cloned())
        .collect();
        let broader = source
            .compile_scalar_region(
                source
                    .compile_between(
                        &entered_at,
                        points.last().unwrap(),
                        &broader_route,
                        broader_route.len(),
                    )
                    .unwrap(),
            )
            .unwrap();
        let mut broader_values = vec![Value::Null; broader.fact_count()];
        for (at, fact) in broader.results() {
            let original = region.results().find(|(point, _)| *point == at).unwrap().1;
            broader_values[fact.index()] = values[original.index()];
        }
        let broader = broader
            .bind_activation(
                &ledger,
                &mut arena,
                heap,
                &invocation,
                &activation,
                ScalarRegionValues {
                    values: &broader_values,
                    calls: &[],
                },
            )
            .unwrap()
            .unwrap();
        assert!(
            call.bind(&exe, call.callee(), &ledger, &mut arena, heap, &broader)
                .unwrap()
                .is_none(),
            "the genuine unpaid head must own the entire composed debt"
        );
        let before = ledger.prospective_invocation(TaskId::MAIN).id();
        let wrong_code = exe
            .islands
            .plans
            .iter()
            .find(|plan| plan.code != call.callee() && exe.code(plan.code).params.len() == 2)
            .unwrap()
            .code;
        let wrong_callee_rejected = call
            .bind(&exe, wrong_code, &ledger, &mut arena, heap, &bound_caller)
            .unwrap()
            .is_none();
        let bound = call
            .bind(
                &exe,
                call.callee(),
                &ledger,
                &mut arena,
                heap,
                &bound_caller,
            )
            .unwrap()
            .unwrap();
        assert!(bound.result().same(&Value::Int(3)));
        // Three evaluated caller operands remain live at call entry. The
        // callee then needs its frame plus two reads and a native answer.
        // Each nonzero word adds one payload byte to its reference.
        assert_eq!(bound.peak(), Cost::FRAME + 6 * (Cost::REFERENCE + 1));
        assert_eq!(bound.delta(), 2 * i128::from(Cost::REFERENCE + 1));
        assert_eq!(bound.depth(), 1);
        let mut parent = call.at().site.clone();
        parent.path.pop();
        let native_at = Point {
            site: parent,
            phase: Phase::Commit,
        };
        let through = Point {
            site: stmt.site.clone(),
            phase: Phase::Commit,
        };
        let route = [&native_at, &through].map(|at| Selection::Event {
            at: at.clone(),
            exit: ExitKind::Normal,
        });
        let suffix = source
            .compile_scalar_region(call.compile_continuation(&exe, &through, &route).unwrap())
            .unwrap();
        let mut suffix_values = vec![Value::Null; suffix.fact_count()];
        let mut saved = None;
        for (slot, fact) in suffix.entries() {
            let value = bound
                .continuation_values()
                .find_map(|(candidate, value)| (candidate == slot).then_some(value))
                .unwrap();
            suffix_values[fact.index()] = value;
            if value.same(&Value::Int(5)) {
                saved = Some(fact);
            }
        }
        for (_, fact) in suffix.results() {
            suffix_values[fact.index()] = Value::Int(8);
        }
        let native_id = suffix
            .interval()
            .events()
            .find(|event| event.point == native_at)
            .unwrap()
            .peak
            .measures()
            .find_map(|measure| match measure {
                crate::compile::room::Measure::CallPeak {
                    callee: crate::compile::room::CallSource::Native(id),
                    ..
                } => Some(*id),
                _ => None,
            })
            .unwrap();
        let lib = exe.library_lib(&native_id).unwrap();
        let crate::compile::LibRun::Native { native, .. } = &exe.lib(lib).run else {
            panic!("native")
        };
        let contract = native
            .resource_contract(lash_kernel_doc::Args::new(
                &[Value::Int(5), Value::Int(3)],
                arena.objects(),
                &arena.semantics(),
            ))
            .unwrap()
            .unwrap();
        let native_rooms = [crate::compile::room::CheckedRoom::native(
            exe.lib(lib).specialization.function,
            &contract,
        )
        .unwrap()];
        let mut wrong_values = suffix_values.clone();
        wrong_values[saved.unwrap().index()] = Value::Int(6);
        let wrong = call
            .bind(
                &exe,
                call.callee(),
                &ledger,
                &mut arena,
                heap,
                &bound_caller,
            )
            .unwrap()
            .unwrap();
        assert!(
            suffix
                .continue_call(
                    wrong,
                    &ledger,
                    &mut arena,
                    heap,
                    &invocation,
                    &activation,
                    ScalarRegionValues {
                        values: &wrong_values,
                        calls: &native_rooms
                    }
                )
                .unwrap()
                .is_none(),
            "a same-price restored caller operand must authenticate against the saved call role"
        );
        let actual = call
            .bind(
                &exe,
                call.callee(),
                &ledger,
                &mut arena,
                heap,
                &bound_caller,
            )
            .unwrap()
            .unwrap();
        let continued = suffix
            .continue_call(
                actual,
                &ledger,
                &mut arena,
                heap,
                &invocation,
                &activation,
                ScalarRegionValues {
                    values: &suffix_values,
                    calls: &native_rooms,
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(continued.peak(), 166);
        drop(continued);
        let call_occurrences = bound.occurrences().to_vec();
        let debt = bound.debt().unwrap();
        // Caller statement plus five expression nodes, then the two body
        // statements and four expression nodes, then 3+2+2+2 native units.
        assert_eq!(debt.units(), 21);
        let tape = crate::observe::ChargeTape::new();
        debt.record(&tape);
        assert_eq!(
            tape.steps().iter().map(|step| step.units()).sum::<u64>(),
            21
        );
        assert_eq!(
            tape.steps()
                .iter()
                .filter(|step| matches!(step, lash_kernel_exec::Weight::Atomic(_)))
                .copied()
                .collect::<Vec<_>>(),
            [lash_kernel_exec::Weight::Atomic(9)]
        );
        let mut charged = 0;
        let mut counters = crate::machine::tasks::Occurrences::for_source_law(1);
        assert!(
            crate::admission::SourceResources {
                charged: &mut charged,
                counters: &mut counters,
                bound: debt.units(),
                slice: debt.units(),
                depth: 1,
                limit: 3,
                safe_point: 0
            }
            .admit_composed(&debt, &[], 1, 0, ())
            .unwrap()
            .is_none(),
            "strict slice equality declines before a physical frame"
        );
        assert_eq!(charged, 0);
        let occurrence = Occurrence {
            task: TaskId::MAIN,
            invocation: invocation.id(),
            loops: vec![],
            action: None,
        };
        let after = Point {
            site: call.source().site.clone(),
            phase: Phase::Leave,
        };
        let recovery = bound.recovery_after(&after, occurrence).unwrap();
        assert!(
            recovery[1].position.unpaid.is_none(),
            "a terminal callee has no unpaid head"
        );
        assert_eq!(
            recovery[0].roles.len(),
            1,
            "the earlier outer argument survives the composed call"
        );
        assert!(
            recovery[0]
                .roles
                .iter()
                .any(|(_, value)| value.same(&Value::Int(5)))
        );
        assert!(std::sync::Arc::ptr_eq(&recovery[0].source, source));
        assert!(std::sync::Arc::ptr_eq(&recovery[1].source, call.source()));
        drop(debt);
        drop(bound);
        assert_eq!(
            ledger.prospective_invocation(TaskId::MAIN).id(),
            before,
            "abandoned binding consumes no invocation"
        );
        let bound = call
            .bind(
                &exe,
                call.callee(),
                &ledger,
                &mut arena,
                heap,
                &bound_caller,
            )
            .unwrap()
            .unwrap();
        let range = bound.finish(&ledger, &invocation).unwrap();
        let prepared = ledger
            .prepare_ownership(&mut arena, heap.pages(), &[], &[])
            .unwrap()
            .complete_invocations(range, 1, heap.pages())
            .unwrap();
        ledger
            .commit(prepared.admit(u64::MAX).ok().unwrap())
            .unwrap();
        assert_eq!(ledger.prospective_invocation(TaskId::MAIN).id(), before + 1);
        assert!(
            call.bind(
                &exe,
                call.callee(),
                &ledger,
                &mut arena,
                heap,
                &bound_caller
            )
            .unwrap()
            .is_none(),
            "stale caller facts cannot authorize the next call"
        );
        // One loop range names every accepted call; abandoned candidates and
        // unused capacity consume no identities or source ownership edits.
        let activation = placement
            .capture(caller.code, &invocation, &ledger, heap, &physical)
            .unwrap()
            .unwrap();
        let roles: Vec<_> = points
            .iter()
            .map(|at| {
                let owner = source
                    .owner_slots()
                    .unwrap()
                    .root(
                        &crate::compile::room::Owner::Operand(at.site.clone()),
                        TaskId::MAIN,
                        invocation.id(),
                    )
                    .unwrap();
                let lash_kernel_state::kernel_internals::logical::Owner::Value { slot, .. } = owner
                else {
                    panic!("caller operand");
                };
                let fact = region.results().find(|(point, _)| *point == at).unwrap().1;
                (slot, values[fact.index()])
            })
            .collect();
        assert!(
            call.scope_roles()
                .all(|slot| roles.iter().any(|(actual, _)| *actual == slot))
        );
        let start = ledger.prospective_invocation(TaskId::MAIN).id();
        let foreign = Ledger::fresh()
            .reserve_invocations(TaskId::MAIN, 4)
            .unwrap();
        assert!(
            call.bind_scope(
                &exe,
                call.callee(),
                &ledger,
                &invocation,
                &activation,
                foreign
            )
            .is_none(),
            "a foreign range cannot authorize this loop"
        );
        let reservation = ledger.reserve_invocations(TaskId::MAIN, 4).unwrap();
        let mut scope = call
            .bind_scope(
                &exe,
                call.callee(),
                &ledger,
                &invocation,
                &activation,
                reservation,
            )
            .unwrap();
        assert!(
            call.evaluate_scoped(&exe, wrong_code, &scope, 0, &arena, &roles)
                .unwrap()
                .is_none(),
            "the resolved callee stays exact inside a scope"
        );
        let future = call
            .evaluate_scoped(&exe, call.callee(), &scope, 1, &arena, &roles)
            .unwrap()
            .unwrap();
        assert_eq!(
            scope.complete(future),
            Err(Refused::Source),
            "completion cannot skip an invocation offset"
        );
        assert_eq!(scope.completed(), 0);
        assert!(
            call.evaluate_scoped(&exe, call.callee(), &scope, 0, &arena, &roles[1..])
                .unwrap()
                .is_none(),
            "saved caller roles cannot disappear inside a loop"
        );
        let occurrence = Occurrence {
            task: TaskId::MAIN,
            invocation: invocation.id(),
            loops: vec![],
            action: None,
        };
        for offset in 0..2 {
            let candidate = call
                .evaluate_scoped(&exe, call.callee(), &scope, offset, &arena, &roles)
                .unwrap()
                .unwrap();
            assert!(candidate.result().same(&Value::Int(3)));
            assert_eq!(candidate.invocation().id(), start + offset);
            assert_eq!(candidate.peak(), Cost::FRAME + 3 * (Cost::REFERENCE + 1));
            assert_eq!(candidate.delta(), -i128::from(Cost::REFERENCE + 1));
            assert_eq!(candidate.depth(), 1);
            assert_eq!(candidate.occurrences(), call_occurrences);
            assert_eq!(candidate.debt().unwrap().units(), 21);
            let recovery = candidate
                .recovery_after(&after, occurrence.clone())
                .unwrap();
            assert!(recovery[1].position.unpaid.is_none());
            assert_eq!(recovery[1].position.occurrence.invocation, start + offset);
            assert!(
                recovery[0]
                    .roles
                    .iter()
                    .any(|(_, value)| value.same(&Value::Int(5)))
            );
            let duplicate = call
                .evaluate_scoped(&exe, call.callee(), &scope, offset, &arena, &roles)
                .unwrap()
                .unwrap();
            scope.complete(candidate).unwrap();
            let room = duplicate.room();
            assert!(
                room.checked(
                    &ledger,
                    &arena,
                    source,
                    call.at(),
                    &invocation,
                    duplicate.invocation()
                )
                .is_none(),
                "a completed invocation cannot authorize its duplicate room"
            );

            assert_eq!(
                scope.complete(duplicate),
                Err(Refused::Source),
                "a candidate cannot complete the same reserved invocation twice"
            );
        }
        let abandoned = call
            .evaluate_scoped(&exe, call.callee(), &scope, 2, &arena, &roles)
            .unwrap()
            .unwrap();
        drop(abandoned);
        assert_eq!(scope.completed(), 2);
        let (reservation, completed) = scope.finish().unwrap();
        let prepared = ledger
            .prepare_ownership(&mut arena, heap.pages(), &[], &[])
            .unwrap()
            .complete_invocations(reservation, completed, heap.pages())
            .unwrap();
        ledger
            .commit(prepared.admit(u64::MAX).ok().unwrap())
            .unwrap();
        assert_eq!(
            ledger.prospective_invocation(TaskId::MAIN).id(),
            start + 2,
            "only the admitted range prefix is consumed"
        );
        let stale = ledger.reserve_invocations(TaskId::MAIN, 4).unwrap();
        ledger.handoff().unwrap();
        let activation = placement
            .capture(caller.code, &invocation, &ledger, heap, &physical)
            .unwrap()
            .unwrap();
        assert!(
            call.bind_scope(
                &exe,
                call.callee(),
                &ledger,
                &invocation,
                &activation,
                stale
            )
            .is_none(),
            "an old range cannot survive a source handoff"
        );
        call.understate_frame_for_law();
        let frame_rejected = call.verify(&exe) == Err(Refused::Peak);
        let mut call = VerifiedCall::compile(&exe, caller.code, statement, pc).unwrap();
        call.omit_caller_role_for_law();
        let recovery_rejected = call.verify(&exe) == Err(Refused::Exit);
        let mut call = VerifiedCall::compile(&exe, caller.code, statement, pc).unwrap();
        call.reorder_closes_for_law();
        let order_rejected = call.verify(&exe) == Err(Refused::Exit);
        assert_eq!(
            [
                ("wrong callee", wrong_callee_rejected),
                ("understated logical frame", frame_rejected),
                ("missing caller recovery", recovery_rejected),
                ("reordered source closes", order_rejected),
            ],
            [
                ("wrong callee", true),
                ("understated logical frame", true),
                ("missing caller recovery", true),
                ("reordered source closes", true),
            ]
        );
    }
    eprintln!(
        "pure-call compiler: 4 layouts bound before frame entry; 0 runtime composition claims"
    );
}

/// Formal padding belongs to the callee's logical entry even when no physical
/// frame exists. Formula bodies bill only completion; fallthrough returns null.
fn entry_calls() {
    for (signature, metering, body, expected, peak, fee) in [
        (
            "callee(a?: Any)",
            "charge sum(7, size(a), size(result))",
            "return a",
            Value::Absent,
            Cost::FRAME + 2 * Cost::REFERENCE,
            11,
        ),
        (
            "callee()",
            "metering statements",
            "",
            Value::Null,
            Cost::FRAME + Cost::REFERENCE,
            2,
        ),
    ] {
        let definition = super::super::super::embedder::fragment(&format!(
            "function {signature} -> Any\nkernel 1\n{metering}\nbody {{ {body} }}"
        ));
        let caller = super::super::super::embedder::fragment(
            "function caller() -> Any\nkernel 1\nmetering statements\nbody { return invoke callee() }",
        );
        let text = format!(
            "definition \"{definition}\"\ndefinition \"{caller}\"\nmain {{ return invoke caller() }}"
        );
        let run = Embedder::with(&text, Setup::default());
        let (program, _) = run.program(true);
        let admitted = program
            .library
            .admit(&program.document, Default::default(), &program.selection)
            .unwrap_or_else(|_| panic!("zero-argument call source admitted"));
        let exe = admitted
            .executable(
                &program.document,
                &program.library,
                &program.documents,
                &program.functions,
                &program.instances,
            )
            .unwrap_or_else(|_| panic!("zero-argument call executable"));
        let caller = exe
            .islands
            .plans
            .iter()
            .find(|plan| {
                exe.code(plan.code).source_room.params.is_empty()
                    && plan.body.statements.len() == 1
                    && exe.code(plan.code).site.unit != lash_kernel_value::sites::Unit::Main
            })
            .unwrap();
        let statement = caller.body.statements[0].id;
        let (stream, stmt, _) = exe.statement(statement);
        let pc = (stmt.ops.start..stmt.ops.end())
            .find(|pc| matches!(stream.ops[*pc as usize], Op::Nested { .. }))
            .unwrap();
        let call = VerifiedCall::compile(&exe, caller.code, statement, pc).unwrap();
        let mut ledger = Ledger::fresh();
        let mut arena = Arena::new();
        let heap = run.machine.source_law_heap();
        let (invocation, entered) = ledger
            .prepare_enter(TaskId::MAIN, &mut arena, heap.pages(), &[], &[])
            .unwrap();
        ledger
            .commit(entered.admit(u64::MAX).ok().unwrap())
            .unwrap();
        let placement = BindingPlacement::compile(&exe, caller.code).unwrap();
        let physical: Vec<_> = (0..exe.code(caller.code).slots.len())
            .map(|_| SlotState::Empty)
            .collect();
        let activation = placement
            .capture(caller.code, &invocation, &ledger, heap, &physical)
            .unwrap()
            .unwrap();
        let position = crate::compile::room::publication::Position {
            at: call.at().clone(),
            unpaid: Some(call.at().clone()),
            recovery: [0; 32],
            occurrence: Occurrence {
                task: TaskId::MAIN,
                invocation: invocation.id(),
                loops: vec![],
                action: None,
            },
        };
        let bound = call
            .bind_entry(
                &exe,
                call.callee(),
                &ledger,
                &mut arena,
                heap,
                &invocation,
                &activation,
                &position,
            )
            .unwrap()
            .unwrap();
        assert!(bound.result().same(&expected));
        assert_eq!(bound.peak(), peak);
        assert_eq!(bound.delta(), i128::from(Cost::REFERENCE));
        assert_eq!(
            bound.debt().unwrap().units(),
            fee,
            "one caller statement and one invoke node, followed by the declaration's completion formula"
        );
    }
}

/// Guarded helper paths must select the real answer and retain the original
/// root action, native/body identity and every source-local charge close.
fn guarded_calls() {
    for layout in [0, 1, 42, u64::MAX] {
        for rooted in [false, true] {
            for (a, b, expected) in [(1, 4, 5), (4, 1, 4)] {
                for (target, native) in [
                    ("helper", false),
                    ("law.guarded", true),
                    ("law.kind_guarded", true),
                    ("law.wrong", true),
                ] {
                    let definition = super::super::super::embedder::fragment(
                        "function helper(a: Any, b: Any) -> Any\nkernel 1\nmetering statements\nbody { if flat.lt(a, b) { return flat.add(a, b) } return flat.mul(a, b) }",
                    );
                    let rhs = if rooted {
                        format!("invoke {target}({a}, {b})")
                    } else {
                        format!("flat.identity(invoke {target}({a}, {b}))")
                    };
                    let caller = super::super::super::embedder::fragment(&format!(
                        "function caller() -> Any\nkernel 1\nmetering statements\nbody {{ let answer = {rhs} return answer }}"
                    ));
                    let text = format!(
                        "definition \"{definition}\"\ndefinition \"{caller}\"\nmain {{ return invoke caller() }}"
                    );
                    let run = Embedder::with(
                        &text,
                        Setup {
                            layout: crate::Layout(layout),
                            probes: Some(guarded_probes),
                            ..Setup::default()
                        },
                    );
                    let (program, _) = run.program(true);
                    let admitted = program
                        .library
                        .admit(&program.document, Default::default(), &program.selection)
                        .unwrap_or_else(|_| panic!("guarded source admitted"));
                    let exe = admitted
                        .executable(
                            &program.document,
                            &program.library,
                            &program.documents,
                            &program.functions,
                            &program.instances,
                        )
                        .unwrap_or_else(|_| panic!("guarded executable"));
                    let caller = exe
                        .islands
                        .plans
                        .iter()
                        .find(|plan| {
                            exe.code(plan.code).source_room.params.is_empty()
                                && plan.body.statements.len() == 2
                        })
                        .unwrap();
                    let statement = caller.body.statements[0].id;
                    let (stream, stmt, _) = exe.statement(statement);
                    let pc = (stmt.ops.start..stmt.ops.end())
                        .find(|pc| matches!(stream.ops[*pc as usize], Op::Nested { .. } | Op::Act))
                        .unwrap();
                    let call = VerifiedCall::compile(&exe, caller.code, statement, pc)
                        .unwrap_or_else(|error| {
                            panic!("guarded {target} root={rooted}: {error:?}")
                        });
                    assert_eq!(call.verify(&exe), Ok(()));
                    let mut ledger = Ledger::fresh();
                    let mut arena = Arena::new();
                    let heap = run.machine.source_law_heap();
                    let (invocation, entered) = ledger
                        .prepare_enter(TaskId::MAIN, &mut arena, heap.pages(), &[], &[])
                        .unwrap();
                    ledger
                        .commit(entered.admit(u64::MAX).ok().unwrap())
                        .unwrap();
                    let placement = BindingPlacement::compile(&exe, caller.code).unwrap();
                    let physical: Vec<_> = (0..exe.code(caller.code).slots.len())
                        .map(|_| SlotState::Empty)
                        .collect();
                    let activation = placement
                        .capture(caller.code, &invocation, &ledger, heap, &physical)
                        .unwrap()
                        .unwrap();
                    let reservation = ledger.reserve_invocations(TaskId::MAIN, 2).unwrap();
                    let scope = call
                        .bind_scope(
                            &exe,
                            call.callee(),
                            &ledger,
                            &invocation,
                            &activation,
                            reservation,
                        )
                        .unwrap();
                    let roles: Vec<_> = call
                        .scope_roles()
                        .zip([Value::Int(a), Value::Int(b)])
                        .collect();
                    let candidate = call
                        .evaluate_scoped(&exe, call.callee(), &scope, 0, &arena, &roles)
                        .unwrap();
                    if target == "law.wrong" {
                        assert!(
                            candidate.is_none(),
                            "an installed native answer cannot be replaced by an equal-price source answer"
                        );
                        continue;
                    }
                    let candidate = candidate.unwrap_or_else(|| {
                        panic!("guarded evaluation {target} root={rooted}, {a},{b}")
                    });
                    assert!(
                        candidate.result().same(&Value::Int(expected)),
                        "actual branch result {target}, {a},{b}"
                    );
                    if rooted {
                        let swapped: Vec<_> = call
                            .scope_roles()
                            .zip([Value::Int(b), Value::Int(a)])
                            .collect();
                        assert!(
                            call.evaluate_scoped(&exe, call.callee(), &scope, 0, &arena, &swapped)
                                .unwrap()
                                .is_none(),
                            "statement-root literals must keep their source order and values"
                        );
                    }
                    let room = candidate.room();
                    assert!(
                        room.checked(
                            &ledger,
                            &arena,
                            &exe.code(caller.code).source_room,
                            call.at(),
                            &invocation,
                            candidate.invocation()
                        )
                        .is_some()
                    );
                    let wrong_invocation = ledger
                        .reserve_invocations(TaskId::MAIN, 2)
                        .unwrap()
                        .invocation(1)
                        .unwrap();
                    assert!(
                        room.checked(
                            &ledger,
                            &arena,
                            &exe.code(caller.code).source_room,
                            call.at(),
                            &invocation,
                            &wrong_invocation
                        )
                        .is_none(),
                        "same function at another invocation has no room authority"
                    );
                    assert!(
                        room.checked(
                            &ledger,
                            &arena,
                            call.source(),
                            call.at(),
                            &invocation,
                            candidate.invocation()
                        )
                        .is_none(),
                        "caller and callee Sources cannot be interchanged"
                    );
                    let debt = candidate.debt().unwrap();
                    // Two native scalar formulas cost 8 for comparison and 9
                    // for arithmetic. Source statements/forms cost 8. A root
                    // caller pays its statement plus its two atom reads.
                    let expected_fee =
                        if native { 7 + 2 + 2 + 2 } else { 8 + 8 + 9 } + if rooted { 3 } else { 5 };
                    assert_eq!(
                        debt.units(),
                        expected_fee,
                        "canonical {target} action={rooted} charges"
                    );
                    let tape = crate::observe::ChargeTape::new();
                    debt.record(&tape);
                    assert_eq!(
                        tape.steps().iter().map(|step| step.units()).sum::<u64>(),
                        expected_fee
                    );
                    let after = Point {
                        site: call.source().site.clone(),
                        phase: Phase::Leave,
                    };
                    let recovery = candidate
                        .recovery_after(
                            &after,
                            Occurrence {
                                task: TaskId::MAIN,
                                invocation: invocation.id(),
                                loops: vec![],
                                action: None,
                            },
                        )
                        .unwrap();
                    assert!(recovery[1].position.unpaid.is_none());
                    assert_eq!(
                        scope.completed(),
                        0,
                        "evaluating room and debt must consume no invocation"
                    );
                    drop(scope);
                    ledger.handoff().unwrap();
                    assert!(
                        room.checked(
                            &ledger,
                            &arena,
                            &exe.code(caller.code).source_room,
                            call.at(),
                            &invocation,
                            candidate.invocation()
                        )
                        .is_none(),
                        "a source call envelope cannot survive a ledger handoff"
                    );
                }
            }
        }
    }
}

fn guarded_probes(
    registry: &mut lash_kernel_doc::FunctionRegistry,
    ids: &std::collections::BTreeMap<&'static str, lash_kernel_value::names::FunctionId>,
) -> Vec<(&'static str, lash_kernel_value::names::FunctionId)> {
    use lash_kernel_doc::{NativeCall, NativeError, NativeFunction, PreparedNative};
    struct Guarded(bool);
    impl Guarded {
        fn answer(args: &[Value]) -> Option<Value> {
            match args {
                [Value::Int(a), Value::Int(b)] => if a < b {
                    a.checked_add(*b)
                } else {
                    a.checked_mul(*b)
                }
                .map(Value::Int),
                _ => None,
            }
        }
    }
    impl NativeFunction for Guarded {
        fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
            if self.0 {
                Ok(Value::Int(99))
            } else {
                Self::answer(call.args.values())
                    .ok_or_else(|| NativeError::refused("scalar", "outside guarded domain"))
            }
        }
        fn prepare(&self) -> Option<PreparedNative> {
            Some(PreparedNative {
                pair: None,
                charged_entry: None,
                operation: "law.guarded",
                entry: if self.0 {
                    |_| Some(Value::Int(99))
                } else {
                    Self::answer
                },
            })
        }
        fn resource_contract(
            &self,
            args: lash_kernel_doc::Args<'_>,
        ) -> Result<Option<lash_kernel_doc::NativeResourceContract>, NativeError> {
            let value = if self.0 {
                Some(Value::Int(99))
            } else {
                Self::answer(args.values())
            };
            value
                .map(|value| lash_kernel_doc::NativeResourceContract::returned(args, &value))
                .transpose()
        }
    }
    let mut functions = super::super::super::layout::scalar_probes(registry, ids);
    for (definition, native) in lash_kernel_lib::numbers() {
        let name = match definition.name.as_str() {
            "kind" => "kind",
            "same" => "same",
            _ => continue,
        };
        functions.push((name, registry.register(definition, Some(native)).unwrap()));
    }
    let get = |name| functions.iter().find(|(key, _)| *key == name).unwrap().1;
    let lt = get("flat.lt");
    let add = get("flat.add");
    let mul = get("flat.mul");
    let kind = get("kind");
    let same = get("same");
    for (name, wrong) in [
        ("law.guarded", false),
        ("law.kind_guarded", false),
        ("law.wrong", true),
    ] {
        let body = if name == "law.kind_guarded" {
            "if same(kind(a), \"integer\") { if same(kind(b), \"integer\") { if flat.lt(a,b) { return flat.add(a,b) } return flat.mul(a,b) } } return absent"
        } else {
            "if flat.lt(a,b) { return flat.add(a,b) } return flat.mul(a,b)"
        };
        let definition = lash_kernel_doc::parse_definition(&format!(
            "function {name}(a: Any, b: Any) -> Any\nkernel 1\ncharge sum(7, size(a), size(b), size(result))\nuse kind = @{kind}\nuse same = @{same}\nuse flat.lt = @{lt}\nuse flat.add = @{add}\nuse flat.mul = @{mul}\nnative\nbody {{ {body} }}"
        )).unwrap();
        functions.push((
            name,
            registry
                .register(definition, Some(std::sync::Arc::new(Guarded(wrong))))
                .unwrap(),
        ));
    }
    functions
}
