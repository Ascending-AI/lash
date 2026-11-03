//! Private capture writes share a shadow, preserve every accepted prefix and
//! consume actual observer exclusion, canonical fees and interior room.
use super::*;
use crate::admission::SourceResources;
use crate::compile::room::{
    placement::BindingPlacement,
    publication::{Occurrence, member_region::MemberRegion},
};
use crate::machine::{SlotState, islands::member_region as runtime};
use lash_kernel_doc::{self as doc, LogicalHeldV1 as Cost, TaskId, Value};
use lash_kernel_state::{
    arena::Arena,
    kernel_internals::logical::{Ledger, Owner, RootValue, Slot},
};
use std::sync::Arc;

pub(super) fn captures() {
    let mut published = (0, 0);
    for layout in [0, 1, 42] {
        for tape in [false, true] {
            let source = crate::laws::layout::scalar_document(
                "let n = 1 let m = 1 let view = fn() { set n = flat.add(n, 1) set m = flat.add(m, 256) set n = flat.add(m, 1) return (n,m) } return view",
            );
            let run = Embedder::with(
                &source,
                Setup {
                    layout: crate::Layout(layout),
                    probes: Some(crate::laws::layout::scalar_probes),
                    ..Setup::default()
                },
            );
            let (mut program, library) = run.program(tape);
            program.library = crate::PreparedLibrary::with_layout(
                Arc::clone(&library.registry),
                crate::Layout(layout),
            );
            let admitted = program
                .library
                .admit(&program.document, Default::default(), &program.selection)
                .ok()
                .expect("admitted source");
            let exe = admitted
                .executable(
                    &program.document,
                    &program.library,
                    &program.documents,
                    &program.functions,
                    &program.instances,
                )
                .ok()
                .expect("source executable");
            let code = exe
                .islands
                .plans
                .iter()
                .find(|plan| {
                    let body = exe.code(plan.code);
                    body.capture_names.iter().any(|name| name.as_str() == "n")
                        && body.capture_names.iter().any(|name| name.as_str() == "m")
                })
                .expect("captured source body")
                .code;
            let body = exe.code(code);
            let placement = Arc::new(BindingPlacement::compile(&exe, code).unwrap());
            let source = Arc::clone(placement.source());
            let slot = |name: &str| {
                let index = body
                    .slots
                    .iter()
                    .position(|slot| slot.name.as_str() == name)
                    .unwrap() as u32;
                let Owner::Value { slot, .. } =
                    source.binding_root(index, TaskId::MAIN, 0).unwrap()
                else {
                    panic!("capture source owner");
                };
                (slot, placement.physical(slot).unwrap())
            };
            let (n, a) = slot("n");
            let (m, b) = slot("m");
            assert!(matches!(n, Slot::Capture(_)) && matches!(m, Slot::Capture(_)));
            let statements = &exe.block(body.body).stmts[..3];
            let plan = MemberRegion::compile(&exe, Arc::clone(&placement), statements, &[a, b])
                .expect("private capture region");
            assert!(
                MemberRegion::compile_tail(
                    &exe,
                    Arc::clone(&placement),
                    &exe.block(body.body).stmts
                )
                .is_some()
            );
            assert!(
                MemberRegion::compile(&exe, Arc::clone(&placement), statements, &[a]).is_err(),
                "every read/write capture needs an authenticated observer footprint role"
            );
            let mut wrong =
                MemberRegion::compile(&exe, Arc::clone(&placement), statements, &[a, b]).unwrap();
            wrong.replace_cell_for_law(m);
            assert!(
                wrong.verify(&exe).is_err(),
                "same-price wrong source capture is not a verified write"
            );
            let mut understated =
                MemberRegion::compile(&exe, Arc::clone(&placement), statements, &[a, b]).unwrap();
            understated.understate_peak_for_law();
            assert!(
                understated.verify(&exe).is_err(),
                "an understated interior peak cannot verify"
            );
            let (_, first_statement, _) = exe.statement(statements[0]);
            let first_charges = crate::compile::room::charges::ChargeProgram::compile(
                &exe,
                code,
                statements[0],
                first_statement.ops,
            )
            .unwrap();
            let first_arguments = [Value::Int(1), Value::Int(1)];
            let first_fee = first_charges
                .bind(&[crate::compile::room::charges::FormulaValues {
                    arguments: &first_arguments,
                    result: Value::Int(2),
                }])
                .unwrap()
                .units();
            let heap = run.machine.source_law_heap();
            for mode in 0..10 {
                let mut arena = Arena::new();
                let cell = arena.new_cell(Value::Int(1));
                let foreign = arena.new_cell(Value::Int(1));
                let store = heap.logical_ledger();
                let mut ledger = store.borrow_mut();
                *ledger = Ledger::fresh();
                let (invocation, entered) = ledger
                    .prepare_enter(
                        TaskId::MAIN,
                        &mut arena,
                        heap.pages(),
                        &[],
                        &[(n, RootValue::Capture(cell)), (m, RootValue::Capture(cell))],
                    )
                    .unwrap();
                ledger
                    .commit(entered.admit(u64::MAX).ok().unwrap())
                    .unwrap();
                let mut physical = vec![SlotState::Empty; body.slots.len()];
                physical[a as usize] = SlotState::Cell(cell);
                physical[b as usize] = SlotState::Cell(cell);
                let before = ledger.balance().unwrap();
                let before_bytes = before.total().unwrap();
                let entry = plan.start(Occurrence {
                    task: TaskId::MAIN,
                    invocation: invocation.id(),
                    loops: vec![],
                    action: None,
                });
                drop(ledger);
                let mut frontier = source
                    .frontier(
                        &store.borrow(),
                        heap,
                        &invocation,
                        program.link,
                        doc::KernelVersion::One,
                        entry.clone(),
                    )
                    .unwrap();
                let mut ledger = store.borrow_mut();
                let activation = placement
                    .capture(code, &invocation, &ledger, heap, &physical)
                    .unwrap()
                    .unwrap();
                let bound = plan
                    .bind_prefix(&exe, &activation, &invocation, &ledger, &mut arena, heap, 3)
                    .unwrap()
                    .expect("actual aliases bind one shadow");
                // Native additions retain the read and literal while admitting
                // their answer. The final call sees two-byte m plus one-byte 1,
                // after the preceding write grew this same cell by one byte.
                assert_eq!(
                    bound.peak(),
                    3 * Cost::REFERENCE + 6,
                    "all simultaneous source temporaries and cell growth remain in the peak"
                );
                drop(bound);
                if mode == 1 {
                    let escaped = Value::ObjectRef(cell);
                    let saved = ledger
                        .prepare_ownership(
                            &mut arena,
                            heap.pages(),
                            &[],
                            &[(Owner::Saved([9; 32]), RootValue::Value(&escaped))],
                        )
                        .unwrap();
                    ledger.commit(saved.admit(u64::MAX).ok().unwrap()).unwrap();
                }
                if mode == 2 {
                    physical[b as usize] = SlotState::Cell(foreign);
                }
                if mode == 0 {
                    let mut abandoned_charge = 0;
                    let mut abandoned_counts =
                        crate::machine::tasks::Occurrences::for_source_law(0);
                    let resources = SourceResources {
                        charged: &mut abandoned_charge,
                        counters: &mut abandoned_counts,
                        bound: u64::MAX,
                        slice: u64::MAX,
                        depth: 0,
                        limit: 20,
                        safe_point: 0,
                    };
                    let activation = placement
                        .capture(code, &invocation, &ledger, heap, &physical)
                        .unwrap()
                        .unwrap();
                    let bound = plan
                        .bind_prefix(&exe, &activation, &invocation, &ledger, &mut arena, heap, 3)
                        .unwrap()
                        .unwrap();
                    let prepared = bound
                        .prepare(
                            &frontier,
                            &ledger,
                            arena.registration().unwrap(),
                            heap,
                            &resources,
                            u64::MAX,
                        )
                        .unwrap()
                        .unwrap();
                    drop(prepared.admit(resources).unwrap().unwrap());
                    assert_eq!(abandoned_charge, 0);
                    assert_eq!(ledger.balance().unwrap(), before);
                    assert_eq!(frontier.position(), &entry);
                    assert!(arena.cell(cell).unwrap().same(&Value::Int(1)));
                    let activation = placement
                        .capture(code, &invocation, &ledger, heap, &physical)
                        .unwrap()
                        .unwrap();
                    let bound = plan
                        .bind_prefix(&exe, &activation, &invocation, &ledger, &mut arena, heap, 3)
                        .unwrap()
                        .unwrap();
                    arena.cell_set(cell, Value::Int(99)).unwrap();
                    let resources = SourceResources {
                        charged: &mut abandoned_charge,
                        counters: &mut abandoned_counts,
                        bound: u64::MAX,
                        slice: u64::MAX,
                        depth: 0,
                        limit: 20,
                        safe_point: 0,
                    };
                    assert!(
                        bound
                            .prepare(
                                &frontier,
                                &ledger,
                                arena.registration().unwrap(),
                                heap,
                                &resources,
                                u64::MAX
                            )
                            .unwrap()
                            .is_none(),
                        "changed equal-price source cell expires the candidate before acceptance"
                    );
                    assert_eq!(ledger.balance().unwrap(), before);
                    arena.cell_set(cell, Value::Int(1)).unwrap();
                    let activation = placement
                        .capture(code, &invocation, &ledger, heap, &physical)
                        .unwrap()
                        .unwrap();
                    let bound = plan
                        .bind_prefix(&exe, &activation, &invocation, &ledger, &mut arena, heap, 3)
                        .unwrap()
                        .unwrap();
                    let saved = Owner::Saved([10; 32]);
                    let extra = Value::Int(1);
                    let changed = ledger
                        .prepare_ownership(
                            &mut arena,
                            heap.pages(),
                            &[],
                            &[(saved, RootValue::Value(&extra))],
                        )
                        .unwrap();
                    ledger
                        .commit(changed.admit(u64::MAX).ok().unwrap())
                        .unwrap();
                    let resources = SourceResources {
                        charged: &mut abandoned_charge,
                        counters: &mut abandoned_counts,
                        bound: u64::MAX,
                        slice: u64::MAX,
                        depth: 0,
                        limit: 20,
                        safe_point: 0,
                    };
                    assert!(
                        bound
                            .prepare(
                                &frontier,
                                &ledger,
                                arena.registration().unwrap(),
                                heap,
                                &resources,
                                u64::MAX
                            )
                            .unwrap()
                            .is_none(),
                        "a new ledger revision expires the candidate even when the actual cell is unchanged"
                    );
                    let removed = ledger
                        .prepare_ownership(&mut arena, heap.pages(), &[saved], &[])
                        .unwrap();
                    ledger
                        .commit(removed.admit(u64::MAX).ok().unwrap())
                        .unwrap();
                    assert_eq!(ledger.balance().unwrap(), before);
                }
                let mut charged = if mode == 8 { u64::MAX - 1 } else { 0 };
                let initial_charge = charged;
                let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
                let recorder = crate::observe::ChargeTape::new();
                runtime::take_publications();
                let memory = if mode == 3 {
                    before_bytes
                } else if mode == 6 {
                    before_bytes + 3 * Cost::REFERENCE + 5
                } else {
                    u64::MAX
                };
                let charge = if mode == 4 {
                    0
                } else if mode == 5 {
                    first_fee
                } else {
                    u64::MAX
                };
                let completion = runtime::execute(
                    &plan,
                    &exe,
                    (code, &physical),
                    &invocation,
                    &mut frontier,
                    &mut ledger,
                    &mut arena,
                    heap,
                    SourceResources {
                        charged: &mut charged,
                        counters: &mut counters,
                        bound: charge,
                        slice: if mode == 7 { first_fee } else { u64::MAX },
                        depth: if mode == 9 { 20 } else { 0 },
                        limit: 20,
                        safe_point: initial_charge,
                    },
                    memory,
                    tape.then_some(&recorder),
                )
                .unwrap();
                let publications = runtime::take_publications();
                published.0 += publications.0;
                published.1 += publications.1;
                if (1..=4).contains(&mode) || matches!(mode, 7 | 8) {
                    assert!(
                        completion.is_none(),
                        "foreign observer/cell or insufficient exact room cannot publish: mode={mode}"
                    );
                    assert!(arena.cell(cell).unwrap().same(&Value::Int(1)));
                    assert_eq!(charged, initial_charge);
                    assert_eq!(frontier.position(), &entry);
                    assert_eq!(publications, (0, 0));
                } else if mode == 5 {
                    let completion =
                        completion.expect("a finite accepted prefix before the next increment");
                    assert_eq!(completion.statements, 1);
                    assert!(arena.cell(cell).unwrap().same(&Value::Int(2)));
                    assert_eq!(publications, (0, 1));
                    assert_eq!(
                        frontier.position().unpaid.as_ref(),
                        Some(&source.first_event(&exe.stmt(statements[1]).site).unwrap())
                    );
                } else if mode == 6 {
                    let completion =
                        completion.expect("memory room admits exactly the first two increments");
                    assert_eq!(completion.statements, 2);
                    assert!(arena.cell(cell).unwrap().same(&Value::Int(258)));
                    assert_eq!(publications, (1, 1));
                    assert_eq!(
                        frontier.position().unpaid.as_ref(),
                        Some(&source.first_event(&exe.stmt(statements[2]).site).unwrap())
                    );
                } else {
                    let completion = completion.expect("real composed private publication");
                    assert_eq!(completion.statements, 3);
                    assert!(arena.cell(cell).unwrap().same(&Value::Int(259)));
                    assert_eq!(
                        publications,
                        (1, 1),
                        "one transaction must execute the certified private kernel"
                    );
                    assert_eq!(
                        frontier.position().unpaid.as_ref(),
                        Some(
                            &source
                                .first_event(&exe.stmt(exe.block(body.body).stmts[3]).site)
                                .unwrap()
                        )
                    );
                    if tape {
                        assert_eq!(
                            recorder
                                .steps()
                                .iter()
                                .map(|weight| weight.units())
                                .sum::<u64>(),
                            charged
                        );
                    } else {
                        assert!(recorder.steps().is_empty());
                    }
                }
                if matches!(mode, 0 | 5 | 6 | 9) {
                    let after_value = arena.cell(cell).unwrap();
                    let after_charge = charged;
                    let safe_point = charged;
                    let retry = runtime::execute(
                        &plan,
                        &exe,
                        (code, &physical),
                        &invocation,
                        &mut frontier,
                        &mut ledger,
                        &mut arena,
                        heap,
                        SourceResources {
                            charged: &mut charged,
                            counters: &mut counters,
                            bound: u64::MAX,
                            slice: u64::MAX,
                            depth: 0,
                            limit: 20,
                            safe_point,
                        },
                        u64::MAX,
                        None,
                    )
                    .unwrap();
                    assert!(
                        retry.is_none(),
                        "a published prefix cannot replay from its old unpaid head"
                    );
                    assert!(arena.cell(cell).unwrap().same(&after_value));
                    assert_eq!(charged, after_charge);
                }
                assert!(arena.cell(foreign).unwrap().same(&Value::Int(1)));
            }
        }
    }
    assert_eq!(published, (18, 24));
    eprintln!(
        "private captured-cell direct runtime publications: {} composed, {} total",
        published.0, published.1
    );
    graphs();
}

/// A source-private table/list keeps one shadow for its unequal writes while
/// an externally retained child cycle remains shared and unchanged.
fn graphs() {
    for kind in 0..3 {
        let body_text = match kind {
            0 => {
                "let values = [1,1,null] set values[0] = values[2] set values[1] = values[0] set values[0] = 258 return values"
            }
            1 => {
                "let values = map{0:1,1:1,2:null} set values[0] = values[2] set values[1.0] = values[0] set values[0] = 258 return values"
            }
            _ => {
                "let values = {left:1,rght:1,child:null} set values.left = values.child set values.rght = values.left set values.left = 258 return values"
            }
        };
        let run = Embedder::new(&crate::laws::layout::scalar_document(body_text));
        let (program, _) = run.program(true);
        let admitted = program
            .library
            .admit(&program.document, Default::default(), &program.selection)
            .ok()
            .expect("admitted source");
        let exe = admitted
            .executable(
                &program.document,
                &program.library,
                &program.documents,
                &program.functions,
                &program.instances,
            )
            .ok()
            .expect("source executable");
        let code = exe
            .islands
            .plans
            .iter()
            .find(|plan| {
                exe.code(plan.code)
                    .slots
                    .iter()
                    .any(|slot| slot.name.as_str() == "values")
            })
            .unwrap()
            .code;
        let body = exe.code(code);
        let placement = Arc::new(BindingPlacement::compile(&exe, code).unwrap());
        let index = body
            .slots
            .iter()
            .position(|slot| slot.name.as_str() == "values")
            .unwrap() as u32;
        let source = placement.source();
        let Owner::Value { slot, .. } = source.binding_root(index, TaskId::MAIN, 0).unwrap() else {
            panic!("source body binding");
        };
        let at = placement.physical(slot).unwrap();
        let plan = MemberRegion::compile(
            &exe,
            Arc::clone(&placement),
            &exe.block(body.body).stmts[1..4],
            &[at],
        )
        .unwrap();
        let heap = run.machine.source_law_heap();
        let mut arena = Arena::new();
        let cycle = arena.create_list(&[Value::Int(9)]);
        arena.list_set(cycle.handle().unwrap(), 1, cycle).unwrap();
        let left = arena.text("left");
        let right = arena.text("rght");
        let child = arena.text("child");
        let mut nursery = doc::Nursery::unaccounted(arena.objects());
        let value = nursery.allocate(match kind {
            0 => doc::NewObject::List(vec![Value::Int(1), Value::Int(1), cycle]),
            1 => doc::NewObject::Map(vec![
                (Value::Int(0), Value::Int(1)),
                (Value::Int(1), Value::Int(1)),
                (Value::Int(2), cycle),
            ]),
            _ => doc::NewObject::Record(vec![
                (left, Value::Int(1)),
                (right, Value::Int(1)),
                (child, cycle),
            ]),
        });
        arena.install(nursery.into_new()).unwrap();
        let store = heap.logical_ledger();
        let mut ledger = store.borrow_mut();
        *ledger = Ledger::fresh();
        let (invocation, entered) = ledger
            .prepare_enter(
                TaskId::MAIN,
                &mut arena,
                heap.pages(),
                &[],
                &[(slot, RootValue::Value(&value))],
            )
            .unwrap();
        ledger
            .commit(entered.admit(u64::MAX).ok().unwrap())
            .unwrap();
        let outside = ledger
            .prepare_ownership(
                &mut arena,
                heap.pages(),
                &[],
                &[(Owner::Saved([3; 32]), RootValue::Value(&cycle))],
            )
            .unwrap();
        ledger
            .commit(outside.admit(u64::MAX).ok().unwrap())
            .unwrap();
        let before = ledger.balance().unwrap();
        let mut physical = vec![SlotState::Empty; body.slots.len()];
        physical[at as usize] = SlotState::Value(value);
        drop(ledger);
        let mut frontier = source
            .frontier(
                &store.borrow(),
                heap,
                &invocation,
                program.link,
                doc::KernelVersion::One,
                plan.start(Occurrence {
                    task: TaskId::MAIN,
                    invocation: invocation.id(),
                    loops: vec![],
                    action: None,
                }),
            )
            .unwrap();
        let mut charged = 0;
        let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
        let tape = crate::observe::ChargeTape::new();
        runtime::take_publications();
        let completed = runtime::execute(
            &plan,
            &exe,
            (code, &physical),
            &invocation,
            &mut frontier,
            &mut store.borrow_mut(),
            &mut arena,
            heap,
            SourceResources {
                charged: &mut charged,
                counters: &mut counters,
                bound: u64::MAX,
                slice: u64::MAX,
                depth: 0,
                limit: 20,
                safe_point: 0,
            },
            u64::MAX,
            Some(&tape),
        )
        .unwrap()
        .expect("source-certified final body");
        assert_eq!(completed.statements, 3);
        assert_eq!(runtime::take_publications(), (1, 1));
        assert_eq!(
            completed.balance, before,
            "one extra integer payload byte replaces one reference's payload byte"
        );
        let handle = value.handle().unwrap();
        if kind == 0 {
            assert!(
                arena
                    .item(handle, 0)
                    .unwrap()
                    .unwrap()
                    .same(&Value::Int(258))
            );
            assert!(arena.item(handle, 1).unwrap().unwrap().same(&cycle));
            assert!(arena.item(handle, 2).unwrap().unwrap().same(&cycle));
        } else {
            let entries = arena.entries(handle).unwrap();
            let keys = if kind == 1 {
                [Value::Int(0), Value::Int(1), Value::Int(2)]
            } else {
                [left, right, child]
            };
            for (pair, key) in entries.as_chunks::<2>().0.iter().zip(keys) {
                assert!(
                    pair[0].same(&key),
                    "original key spelling/order is retained"
                );
            }
            assert!(entries[1].same(&Value::Int(258)));
            assert!(entries[3].same(&cycle));
            assert!(entries[5].same(&cycle));
        }
        assert!(
            arena
                .item(cycle.handle().unwrap(), 1)
                .unwrap()
                .unwrap()
                .same(&cycle)
        );
        assert_eq!(
            tape.steps()
                .iter()
                .map(|weight| weight.units())
                .sum::<u64>(),
            charged
        );
    }
}
