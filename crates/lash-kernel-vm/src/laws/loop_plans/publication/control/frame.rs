//! K-EVAL-008/K-BND-002: formals, source identity and abandoned entry are exact.
use super::super::super::*;
use crate::admission::{Allowance, SourceResources};
use crate::compile::room::{
    Phase, Point,
    publication::{FrameCaller, FrameEntry, Occurrence, Position},
};
use crate::machine::SlotState;
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_state::{
    arena::Arena,
    kernel_internals::logical::{PublicationValue as PV, RootValue, Slot},
};
use std::sync::Arc;

pub(super) fn entries() {
    let run = Embedder::new(
        "main { let f = fn(a, b) { return a } let g = fn(a, b) { return b } return (9, apply f(7)) }",
    );
    let (program, _) = run.program(false);
    let admitted = program
        .library
        .admit(&program.document, Default::default(), &program.selection)
        .unwrap_or_else(|_| panic!("frame source admission"));
    let exe = admitted
        .executable(
            &program.document,
            &program.library,
            &program.documents,
            &program.functions,
            &program.instances,
        )
        .unwrap_or_else(|_| panic!("frame executable"));
    let code_id = exe
        .islands
        .plans
        .iter()
        .find(|plan| {
            let code = exe.code(plan.code);
            code.params.len() == 2
                && code
                    .created
                    .as_ref()
                    .is_some_and(|site| site.path.first() == Some(&0))
        })
        .unwrap()
        .code;
    let code = exe.code(code_id);
    let source = &code.source_room;
    let entry = FrameEntry::compile(&exe, code_id).unwrap();
    let heap = run.machine.source_law_heap();
    let store = heap.logical_ledger();
    let mut arena = Arena::new();
    let mut ledger = store.borrow_mut();
    let (caller, entered) = ledger
        .prepare_enter(TaskId::MAIN, &mut arena, heap.pages(), &[], &[])
        .unwrap();
    ledger.commit(entered.admit(4096).ok().unwrap()).unwrap();
    let caller_code = exe.main.unwrap();
    let caller_source = &exe.code(caller_code).source_room;
    let statement = exe.block(exe.code(caller_code).body).stmts[2];
    let (stream, _, _) = exe.statement(statement);
    let nested = stream
        .nested
        .iter()
        .find(|entry| {
            matches!(
                entry.kind,
                crate::compile::NestedKind::Call(crate::compile::NestedCallee::Read)
            ) && !entry.roles.is_empty()
        })
        .unwrap();
    let call_at = Point {
        site: nested.site.clone(),
        phase: Phase::Enter,
    };
    let enter = caller_source
        .compile_event(&call_at, crate::compile::room::ExitKind::Action)
        .unwrap();
    let caller_slots = caller_source.owner_slots().unwrap();
    let root = |selector: &crate::compile::room::Owner| {
        caller_slots
            .root(selector, TaskId::MAIN, caller.id())
            .unwrap()
    };
    let event = enter.events().next().unwrap();
    let callee_input = root(&event.inputs[0]);
    let input = root(&event.inputs[1]);
    let saved = root(&crate::compile::room::Owner::Operand(
        nested.roles[0].site.clone(),
    ));
    let continuation = caller_slots
        .continuation(&nested.roles[0].site, TaskId::MAIN, caller.id())
        .unwrap();
    let foreign = caller.owner(Slot::Argument(crate::laws::role_id(1)));
    let creation = exe
        .islands
        .plans
        .iter()
        .find(|plan| {
            let code = exe.code(plan.code);
            code.params.len() == 2
                && code
                    .created
                    .as_ref()
                    .is_some_and(|site| site.path.first() == Some(&1))
        })
        .unwrap()
        .code;
    let make_closure = |arena: &mut Arena, target| {
        let code = exe.code(target);
        let site = code.created.as_ref().unwrap();
        let origin = lash_kernel_doc::snapshot::closure_at(
            &program.document,
            site,
            program.link,
            exe.helpers(),
            exe.selection(),
        )
        .unwrap();
        Value::Closure(arena.new_closure(
            Arc::new(lash_kernel_doc::arena::ClosureIdentity {
                capture_names: code.capture_names.clone(),
                callable: lash_kernel_value::CallableId {
                    link: program.link,
                    function: origin.function,
                },
                site: lash_kernel_doc::linked_site::LinkedSite {
                    link: program.link,
                    site: site.clone(),
                },
            }),
            vec![],
        ))
    };
    let callable = make_closure(&mut arena, code_id);
    let wrong_callable = make_closure(&mut arena, creation);
    let held = ledger
        .prepare_ownership(
            &mut arena,
            heap.pages(),
            &[],
            &[
                (input, RootValue::Value(&Value::Int(7))),
                (foreign, RootValue::Value(&Value::Int(9))),
                (callee_input, RootValue::Value(&callable)),
                (saved, RootValue::Value(&Value::Int(9))),
            ],
        )
        .unwrap();
    ledger.commit(held.admit(4096).ok().unwrap()).unwrap();
    let invocation = ledger.prospective_invocation(TaskId::MAIN);
    let balance = ledger.balance().unwrap();
    drop(ledger);
    let ledger = store.borrow();
    let position = Position {
        at: Point {
            site: source.site.clone(),
            phase: Phase::Enter,
        },
        unpaid: Some(source.first_event(&source.site.child(0)).unwrap()),
        recovery: [13; 32],
        occurrence: Occurrence {
            task: TaskId::MAIN,
            invocation: invocation.id(),
            loops: vec![],
            action: None,
        },
    };
    let frontier = source
        .frontier(
            &ledger,
            heap,
            &invocation,
            program.link,
            lash_kernel_doc::KernelVersion::One,
            position.clone(),
        )
        .unwrap();
    let outputs = || {
        code.params
            .iter()
            .enumerate()
            .map(|(index, param)| {
                (
                    source
                        .binding_root(param.slot, TaskId::MAIN, invocation.id())
                        .unwrap(),
                    if index == 0 {
                        PV::Existing(input)
                    } else {
                        PV::Native(Value::Absent)
                    },
                )
            })
            .collect::<Vec<_>>()
    };
    let mut states = vec![SlotState::Empty; code.slots_by_at.len()];
    states[code.params[0].at as usize] = SlotState::Value(Value::Int(7));
    states[code.params[1].at as usize] = SlotState::Value(Value::Absent);
    let arguments = [(Some(input), Value::Int(7))];
    let mut rejected = Vec::new();
    for mutation in 0..5 {
        let mut actual = outputs();
        let mut bindings = states.clone();
        let mut args = arguments;
        match mutation {
            0 => {
                actual.pop();
            }
            1 => {
                bindings[code.params[1].at as usize] = SlotState::Value(Value::Int(9));
            }
            2 => {
                actual[0].1 = PV::Native(Value::Int(7));
            }
            3 => {
                args[0].0 = Some(foreign);
            }
            4 => {
                actual.push(actual[0].clone());
            }
            _ => unreachable!(),
        }
        let (_, installation) = arena.prepare_cells(&[]).unwrap();
        rejected.push(
            entry
                .prepare(
                    source,
                    program.link,
                    &frontier,
                    &invocation,
                    &ledger,
                    installation,
                    heap,
                    &args,
                    None,
                    &bindings,
                    None,
                    &[input],
                    &actual,
                    &[],
                    &[],
                )
                .unwrap()
                .is_none(),
        );
    }
    for mutation in 0..3 {
        let mut actual = outputs();
        actual.push((continuation, PV::Existing(saved)));
        let frame_caller = FrameCaller {
            source: Arc::clone(caller_source),
            invocation: caller.clone(),
            entry: nested,
            callable: Some(if mutation == 0 {
                wrong_callable
            } else {
                callable
            }),
            save: true,
        };
        let mut inputs = vec![input, callee_input, saved];
        if mutation == 1 {
            actual.pop();
        }
        if mutation == 2 {
            inputs.pop();
        }
        let (_, installation) = arena.prepare_cells(&[]).unwrap();
        rejected.push(
            entry
                .prepare(
                    source,
                    program.link,
                    &frontier,
                    &invocation,
                    &ledger,
                    installation,
                    heap,
                    &arguments,
                    None,
                    &states,
                    Some(&frame_caller),
                    &inputs,
                    &actual,
                    &[],
                    &[],
                )
                .unwrap()
                .is_none(),
        );
    }
    let mut proven = source
        .frontier(
            &ledger,
            heap,
            &invocation,
            program.link,
            lash_kernel_doc::KernelVersion::One,
            position.clone(),
        )
        .unwrap();
    proven.proven().unwrap();
    let (_, installation) = arena.prepare_cells(&[]).unwrap();
    rejected.push(
        entry
            .prepare(
                source,
                program.link,
                &proven,
                &invocation,
                &ledger,
                installation,
                heap,
                &arguments,
                None,
                &states,
                None,
                &[input],
                &outputs(),
                &[],
                &[],
            )
            .unwrap()
            .is_none(),
    );
    assert_eq!(
        rejected, [true; 9],
        "formals, padding, provenance, callee identity and saved recovery operands bind one entry"
    );
    let mut actual = outputs();
    actual.push((continuation, PV::Existing(saved)));
    let frame_caller = FrameCaller {
        source: Arc::clone(caller_source),
        invocation: caller.clone(),
        entry: nested,
        callable: Some(callable),
        save: true,
    };
    let (_, installation) = arena.prepare_cells(&[]).unwrap();
    let accepted_caller = entry
        .prepare(
            source,
            program.link,
            &frontier,
            &invocation,
            &ledger,
            installation,
            heap,
            &arguments,
            None,
            &states,
            Some(&frame_caller),
            &[input, callee_input, saved],
            &actual,
            &[],
            &[],
        )
        .unwrap();
    assert!(
        accepted_caller.is_some(),
        "the actual callee and complete caller continuation are admitted"
    );
    drop(accepted_caller);
    let (_, installation) = arena.prepare_cells(&[]).unwrap();
    let (prepared, _batch) = entry
        .prepare(
            source,
            program.link,
            &frontier,
            &invocation,
            &ledger,
            installation,
            heap,
            &arguments,
            None,
            &states,
            None,
            &[input],
            &outputs(),
            &[],
            &[],
        )
        .unwrap()
        .unwrap();
    let peak = prepared.peak();
    assert!(prepared.admit(peak - 1).is_err());
    assert_eq!(ledger.prospective_invocation(TaskId::MAIN), invocation);
    assert_eq!(ledger.balance().unwrap(), balance);
    let mut charged = 7;
    let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
    let allowance = Allowance {
        occurrences: vec![],
        before: 0,
        units: 0,
        close: None,
        strict: false,
        depth: 0,
        safe_point: 7,
    };
    let (_, installation) = arena.prepare_cells(&[]).unwrap();
    let (prepared, _batch) = entry
        .prepare(
            source,
            program.link,
            &frontier,
            &invocation,
            &ledger,
            installation,
            heap,
            &arguments,
            None,
            &states,
            None,
            &[input],
            &outputs(),
            &[],
            &[],
        )
        .unwrap()
        .unwrap();
    let resources = SourceResources {
        charged: &mut charged,
        counters: &mut counters,
        bound: 7,
        slice: 7,
        depth: 0,
        limit: 1,
        safe_point: 7,
    };
    let facts = resources.facts(&allowance).unwrap();
    let accepted = resources
        .admit_interval(&facts, &allowance, prepared.admit(peak).ok().unwrap())
        .unwrap()
        .unwrap();
    drop(accepted);
    assert_eq!(charged, 7);
    assert_eq!(
        ledger.prospective_invocation(TaskId::MAIN),
        invocation,
        "an abandoned acceptance grants no invocation"
    );
    assert_eq!(ledger.balance().unwrap(), balance);
    let (_, installation) = arena.prepare_cells(&[]).unwrap();
    let (mut prepared, _batch) = entry
        .prepare(
            source,
            program.link,
            &frontier,
            &invocation,
            &ledger,
            installation,
            heap,
            &arguments,
            None,
            &states,
            None,
            &[input],
            &outputs(),
            &[],
            &[],
        )
        .unwrap()
        .unwrap();
    drop(ledger);
    let mut ledger = store.borrow_mut();
    ledger.handoff().unwrap();
    assert!(
        !prepared.is_current(&ledger),
        "a prepared frame cannot outlive its entry dependencies"
    );
    drop(prepared);
    assert_eq!(ledger.prospective_invocation(TaskId::MAIN), invocation);
    drop(ledger);
    let ledger = store.borrow();
    let frontier = source
        .frontier(
            &ledger,
            heap,
            &invocation,
            program.link,
            lash_kernel_doc::KernelVersion::One,
            position,
        )
        .unwrap();
    let (_, installation) = arena.prepare_cells(&[]).unwrap();
    let (prepared, batch) = entry
        .prepare(
            source,
            program.link,
            &frontier,
            &invocation,
            &ledger,
            installation,
            heap,
            &arguments,
            None,
            &states,
            None,
            &[input],
            &outputs(),
            &[],
            &[],
        )
        .unwrap()
        .unwrap();
    let accepted = prepared.admit(peak).ok().unwrap();
    drop(ledger);
    accepted
        .publish(&mut store.borrow_mut(), |arena, edits| {
            assert!(edits.is_empty());
            batch.publish(arena);
        })
        .unwrap();
    let ledger = store.borrow();
    assert!(
        ledger
            .invocation(TaskId::MAIN, invocation.id(), heap.pages())
            .is_ok()
    );
    assert_eq!(
        ledger.prospective_invocation(TaskId::MAIN).id(),
        invocation.id() + 1
    );
    assert!(ledger.holding(input, heap.pages()).unwrap().is_none());
}
