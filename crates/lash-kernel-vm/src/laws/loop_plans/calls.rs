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
            definition("caller()", "return flat.add(5, invoke multiply(3, 4))")
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
                    && source.body.len() == 1
                    && matches!(source.body[0], lash_kernel_doc::Stmt::Return { .. })
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
        let wrong = exe
            .islands
            .plans
            .iter()
            .find(|plan| plan.code != call.callee() && exe.code(plan.code).params.len() == 2)
            .unwrap()
            .code;
        let wrong_callee_rejected = call
            .bind(&exe, wrong, &ledger, &mut arena, heap, &bound_caller)
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
            site: call.body().source().site.clone(),
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
        assert!(std::sync::Arc::ptr_eq(
            &recovery[1].source,
            call.body().source()
        ));
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
