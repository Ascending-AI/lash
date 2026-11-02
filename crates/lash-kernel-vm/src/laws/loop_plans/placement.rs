//! Same-price values cannot substitute another physical binding at entry.
use super::*;
use crate::compile::room::{
    ExitKind, Phase, Point, Selection, placement::BindingPlacement, region::ScalarRegionValues,
};
use crate::machine::SlotState;
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_exec::Op;
use lash_kernel_state::{
    arena::Arena,
    kernel_internals::logical::{Ledger, Owner, RootValue},
};

pub(super) fn binding_values() {
    for layout in [0, 1, 42, u64::MAX] {
        let text = super::super::layout::scalar_document(
            "let left = 3 let right = 8 set left = right return left",
        );
        let run = Embedder::with(
            &text,
            Setup {
                layout: crate::Layout(layout),
                ..Setup::default()
            },
        );
        let (program, _) = run.program(true);
        let admitted = program
            .library
            .admit(&program.document, Default::default(), &program.selection)
            .unwrap_or_else(|_| panic!("placement source is admitted"));
        let exe = admitted
            .executable(
                &program.document,
                &program.library,
                &program.documents,
                &program.functions,
                &program.instances,
            )
            .unwrap_or_else(|_| panic!("placement source compiles"));
        let body = exe
            .islands
            .plans
            .iter()
            .find(|plan| plan.params.is_empty() && plan.body.statements.len() == 4)
            .unwrap();
        let placement = BindingPlacement::compile(&exe, body.code).unwrap();
        let source = placement.source();
        let id = body.body.statements[2].id;
        let (stream, statement, _) = exe.statement(id);
        let (pc, local) = (statement.ops.start..statement.ops.end())
            .find_map(|pc| match stream.ops[pc as usize] {
                Op::Local { local, .. } => Some((pc, local)),
                _ => None,
            })
            .unwrap();
        let read = Point {
            site: (**stream.source_sites[pc as usize].as_ref().unwrap()).clone(),
            phase: Phase::Commit,
        };
        assert_eq!(placement.verify_read(&read, local.at), Ok(()));
        let wrong = (0..body.slots as u32).find(|at| *at != local.at).unwrap();
        assert!(
            placement.verify_read(&read, wrong).is_err(),
            "equal scalar prices do not authorize the other lexical binding"
        );
        let through = Point {
            site: statement.site.clone(),
            phase: Phase::Commit,
        };
        let route = [&read, &through].map(|at| Selection::Event {
            at: at.clone(),
            exit: ExitKind::Normal,
        });
        let interval = source
            .compile_between(&read, &through, &route, route.len())
            .unwrap();
        let region = source.compile_scalar_region(interval).unwrap();
        let code = exe.code(body.code);
        let physical: Vec<_> = code
            .slots_by_at
            .iter()
            .map(|index| {
                SlotState::Value(Value::Int(
                    if code.slots[*index as usize].name.as_str() == "left" {
                        3
                    } else {
                        8
                    },
                ))
            })
            .collect();
        let roots: Vec<_> = code
            .slots_by_at
            .iter()
            .enumerate()
            .map(|(at, index)| {
                let Some(Owner::Value { slot, .. }) = source.binding_root(*index, TaskId::MAIN, 0)
                else {
                    panic!("scalar binding");
                };
                let SlotState::Value(value) = &physical[at] else {
                    unreachable!();
                };
                (slot, RootValue::Value(value))
            })
            .collect();
        let heap = run.machine.source_law_heap();
        let mut arena = Arena::new();
        let mut ledger = Ledger::fresh();
        let (invocation, entered) = ledger
            .prepare_enter(TaskId::MAIN, &mut arena, heap.pages(), &[], &roots)
            .unwrap();
        ledger
            .commit(entered.admit(u64::MAX).ok().unwrap())
            .unwrap();
        let activation = placement
            .capture(body.code, &invocation, &ledger, heap, &physical)
            .unwrap()
            .unwrap();
        let mut values = vec![Value::Null; region.fact_count()];
        for (slot, fact) in region.entries() {
            values[fact.index()] = activation.value(slot).unwrap();
        }
        for (_, fact) in region.results() {
            values[fact.index()] = Value::Int(8);
        }
        assert!(
            region
                .bind_activation(
                    &ledger,
                    &mut arena,
                    heap,
                    &invocation,
                    &activation,
                    ScalarRegionValues {
                        values: &values,
                        calls: &[]
                    }
                )
                .unwrap()
                .is_some()
        );
        let forged = vec![Value::Int(99); region.fact_count()];
        // The former price-only binding accepts this. The live activation must
        // reject it even when reads, results and holding sizes agree.
        assert!(
            region
                .bind(
                    &ledger,
                    &mut arena,
                    heap,
                    &invocation,
                    ScalarRegionValues {
                        values: &forged,
                        calls: &[]
                    }
                )
                .unwrap()
                .is_some()
        );
        assert!(
            region
                .bind_activation(
                    &ledger,
                    &mut arena,
                    heap,
                    &invocation,
                    &activation,
                    ScalarRegionValues {
                        values: &forged,
                        calls: &[]
                    }
                )
                .unwrap()
                .is_none(),
            "entry facts must be the actual physical binding values"
        );
        assert!(
            placement
                .capture(
                    crate::compile::CodeId(u32::MAX),
                    &invocation,
                    &ledger,
                    heap,
                    &physical
                )
                .unwrap()
                .is_none()
        );
        let changed = ledger
            .prepare_source_boundary(arena.registration().unwrap(), heap, &[], &[])
            .unwrap();
        changed
            .admit(u64::MAX)
            .ok()
            .unwrap()
            .publish(&mut ledger, |_, _| {})
            .unwrap();
        assert!(
            region
                .bind_activation(
                    &ledger,
                    &mut arena,
                    heap,
                    &invocation,
                    &activation,
                    ScalarRegionValues {
                        values: &values,
                        calls: &[]
                    }
                )
                .unwrap()
                .is_none(),
            "a ledger publication expires the captured activation"
        );
    }
}
