//! K-VMEM-001/K-CHG-002: refused tuple tests install no nursery or paid prefix.
use super::*;
use crate::admission::SourceResources;
use crate::compile::room::{
    self,
    placement::BindingPlacement,
    publication::{Occurrence, UnitComparison},
};
use crate::machine::SlotState;
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_exec::Op;
use lash_kernel_state::{
    arena::Arena,
    kernel_internals::logical::{Ledger, RootValue, Slot},
};
use std::sync::Arc;

pub(super) fn comparisons() {
    for layout in [0, 1, 42] {
        let run = Embedder::with(
            "main { let f = fn(value) { if same(value, ()) { return 0 } return value } return apply f(7) }",
            Setup {
                layout: crate::Layout(layout),
                probes: Some(crate::laws::layout::helper_probes),
                ..Setup::default()
            },
        );
        let (program, _) = run.program(false);
        let admitted = program
            .library
            .admit(&program.document, Default::default(), &program.selection)
            .unwrap_or_else(|_| panic!("unit source admitted"));
        let exe = admitted
            .executable(
                &program.document,
                &program.library,
                &program.documents,
                &program.functions,
                &program.instances,
            )
            .unwrap_or_else(|_| panic!("unit executable"));
        let code = exe
            .islands
            .plans
            .iter()
            .find(|plan| {
                exe.code(plan.code).params.len() == 1 && exe.code(plan.code).created.is_some()
            })
            .unwrap()
            .code;
        let statement = exe.block(exe.code(code).body).stmts[0];
        let (stream, stmt, _) = exe.statement(statement);
        let pc = (stmt.ops.start..stmt.ops.end())
            .find(|pc| matches!(stream.ops[*pc as usize], Op::Operator { .. }))
            .unwrap();
        let close = (stmt.ops.start..pc)
            .filter_map(|head| match stream.ops[head as usize] {
                Op::Region { region, total } => Some(crate::compile::expr::Close {
                    region: region.0,
                    head,
                    total,
                }),
                _ => None,
            })
            .next_back();
        let placement = Arc::new(BindingPlacement::compile(&exe, code).unwrap());
        let unit = UnitComparison::compile(&exe, Arc::clone(&placement), statement, pc, close)
            .unwrap_or_else(|error| {
                panic!(
                    "unit compile at layout {layout}: {error:?}; ops={:?}",
                    &stream.ops[stmt.ops.start as usize..stmt.ops.end() as usize]
                )
            });
        let binding = (stmt.ops.start..pc)
            .find_map(|pc| match stream.ops[pc as usize] {
                Op::Local { local, .. } => Some(local.at),
                _ => None,
            })
            .unwrap();
        let heap = run.machine.source_law_heap();
        let store = heap.logical_ledger();
        for guard in 0..7 {
            let mut arena = Arena::new();
            *store.borrow_mut() = Ledger::fresh();
            let value = Value::Int(7);
            let (invocation, entered) = store
                .borrow()
                .prepare_enter(
                    TaskId::MAIN,
                    &mut arena,
                    heap.pages(),
                    &[],
                    &[(Slot::Binding(0), RootValue::Value(&value))],
                )
                .unwrap();
            store
                .borrow_mut()
                .commit(entered.admit(u64::MAX).ok().unwrap())
                .unwrap();
            let mut physical = vec![SlotState::Empty; exe.code(code).slots.len()];
            physical[binding as usize] = SlotState::Value(value);
            let position = unit
                .start(
                    &store.borrow(),
                    heap,
                    Occurrence {
                        task: TaskId::MAIN,
                        invocation: invocation.id(),
                        loops: vec![],
                        action: None,
                    },
                )
                .unwrap();
            let source = &exe.code(code).source_room;
            let mut frontier = source
                .frontier(
                    &store.borrow(),
                    heap,
                    &invocation,
                    program.link,
                    lash_kernel_doc::KernelVersion::One,
                    position.clone(),
                )
                .unwrap();
            let before = (
                arena.allocations(),
                arena.held(),
                store.borrow().balance().unwrap(),
            );
            assert!(arena.resident_empty_tuple().is_none());
            let mut charged = 7;
            let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
            let published = {
                let resources = SourceResources {
                    charged: &mut charged,
                    counters: &mut counters,
                    bound: u64::MAX,
                    slice: if guard == 4 { 7 } else { u64::MAX },
                    depth: 0,
                    limit: 10,
                    safe_point: 7,
                };
                let ledger = store.borrow();
                let prepared = unit
                    .prepare(
                        &exe,
                        if guard == 2 {
                            crate::compile::CodeId(code.0 + 1)
                        } else {
                            code
                        },
                        &frontier,
                        &invocation,
                        &ledger,
                        heap,
                        &mut arena,
                        &physical,
                        if guard == 1 { Value::Int(8) } else { value },
                        resources,
                        if guard == 3 {
                            before.2.total().unwrap()
                        } else {
                            u64::MAX
                        },
                    )
                    .unwrap();
                assert_eq!(
                    prepared.is_some(),
                    matches!(guard, 0 | 5 | 6),
                    "unit source guard {guard}, layout {layout}"
                );
                drop(ledger);
                match prepared {
                    Some((accepted, debt)) => {
                        if guard == 0 {
                            drop(accepted);
                            false
                        } else {
                            if guard == 5 {
                                store.borrow_mut().handoff().unwrap();
                            }
                            let result =
                                accepted.publish_unit(&mut frontier, &mut store.borrow_mut());
                            if guard == 5 {
                                assert!(result.is_err());
                                false
                            } else {
                                result.unwrap();
                                assert_eq!(charged, 7 + debt.units());
                                true
                            }
                        }
                    }
                    None => false,
                }
            };
            if published {
                assert!(
                    charged > 7,
                    "the comparison must pay its actual fixed events and completion"
                );
                assert!(arena.resident_empty_tuple().is_some());
                assert_eq!(
                    frontier.position().unpaid.as_ref(),
                    Some(&room::Point {
                        site: stmt.site.clone(),
                        phase: room::Phase::Test
                    })
                );
                continue;
            }
            assert_eq!(charged, 7, "misses cannot pay a source prefix");
            assert_eq!(
                (
                    arena.allocations(),
                    arena.held(),
                    store.borrow().balance().unwrap()
                ),
                before,
                "misses cannot install a tuple or change logical roots"
            );
            assert!(arena.resident_empty_tuple().is_none());
            assert_eq!(frontier.position(), &position);
        }
    }
}
