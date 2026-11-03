//! Private member shadows require the complete canonical observer footprint.
use super::*;
use crate::compile::room::{placement::BindingPlacement, publication::observers::ObserverScope};
use crate::machine::SlotState;
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_state::{
    arena::Arena,
    kernel_internals::logical::{Ledger, Owner, RootValue, Slot},
};
use std::sync::Arc;

pub(super) fn isolation() {
    for layout in [0, 1, 42] {
        let text = crate::laws::layout::scalar_document(
            "let n = 1 let m = 2 let view = fn() { return (n, m) } return view",
        );
        let run = Embedder::with(
            &text,
            Setup {
                layout: crate::Layout(layout),
                ..Setup::default()
            },
        );
        let (mut program, library) = run.program(false);
        program.library = crate::PreparedLibrary::with_layout(
            Arc::clone(&library.registry),
            crate::Layout(layout),
        );
        let admitted = program
            .library
            .admit(&program.document, Default::default(), &program.selection)
            .unwrap_or_else(|_| panic!("observer source admitted"));
        let exe = admitted
            .executable(
                &program.document,
                &program.library,
                &program.documents,
                &program.functions,
                &program.instances,
            )
            .unwrap_or_else(|_| panic!("observer source compiled"));
        let code = exe
            .islands
            .plans
            .iter()
            .find(|plan| {
                let slots = &exe.code(plan.code).slots;
                ["n", "m", "view"]
                    .iter()
                    .all(|name| slots.iter().any(|slot| slot.name.as_str() == *name))
            })
            .unwrap()
            .code;
        let body = exe.code(code);
        let placement = Arc::new(BindingPlacement::compile(&exe, code).unwrap());
        let source = placement.source();
        let slot = |name: &str| {
            let index = body
                .slots
                .iter()
                .position(|binding| binding.name.as_str() == name)
                .unwrap() as u32;
            let Owner::Value { slot, .. } = source.binding_root(index, TaskId::MAIN, 0).unwrap()
            else {
                panic!("source binding root");
            };
            (slot, placement.physical(slot).unwrap())
        };
        let (first, a) = slot("n");
        let (second, b) = slot("m");
        let scope = ObserverScope::compile(Arc::clone(&placement), &[a, b], &[]).unwrap();
        let partial = ObserverScope::compile(Arc::clone(&placement), &[a], &[]).unwrap();
        assert!(ObserverScope::compile(Arc::clone(&placement), &[a, a], &[]).is_err());
        assert!(ObserverScope::compile(Arc::clone(&placement), &[u32::MAX], &[]).is_err());
        let heap = run.machine.source_law_heap();
        let mut ledger = Ledger::fresh();
        let mut arena = Arena::new();
        let cell = arena.new_cell(Value::Int(1));
        let wrong = arena.new_cell(Value::Int(1));
        let value = Value::ObjectRef(cell);
        let (invocation, entered) = ledger
            .prepare_enter(
                TaskId::MAIN,
                &mut arena,
                heap.pages(),
                &[],
                &[
                    (first, RootValue::Value(&value)),
                    (second, RootValue::Value(&value)),
                ],
            )
            .unwrap();
        ledger
            .commit(entered.admit(u64::MAX).ok().unwrap())
            .unwrap();
        let mut physical = vec![SlotState::Empty; body.slots.len()];
        physical[a as usize] = SlotState::Cell(cell);
        physical[b as usize] = SlotState::Cell(cell);
        let bind = |scope: &ObserverScope,
                    ledger: &Ledger,
                    arena: &mut Arena,
                    physical: &[SlotState],
                    target| {
            let activation = placement
                .capture(code, &invocation, ledger, heap, physical)
                .unwrap()
                .unwrap();
            scope
                .bind(
                    &activation,
                    &invocation,
                    ledger,
                    arena,
                    heap,
                    &[],
                    &[target],
                )
                .unwrap()
        };
        assert!(
            bind(&partial, &ledger, &mut arena, &physical, cell).is_none(),
            "every alias must be in the source footprint"
        );
        assert!(
            bind(&scope, &ledger, &mut arena, &physical, wrong).is_none(),
            "equal-price foreign cell is not private"
        );
        physical[b as usize] = SlotState::Cell(wrong);
        assert!(
            bind(&scope, &ledger, &mut arena, &physical, cell).is_none(),
            "actual activation alias must match its root"
        );
        physical[b as usize] = SlotState::Cell(cell);
        let before = ledger.balance().unwrap();
        let old = arena.content_version(cell).unwrap();
        let proof = bind(&scope, &ledger, &mut arena, &physical, cell).unwrap();
        assert!(proof.permits(source, &invocation, &ledger, &arena, cell));
        assert!(
            !proof.permits(source, &invocation, &Ledger::fresh(), &arena, cell),
            "observer proof cannot cross a ledger identity"
        );
        assert!(!proof.permits(source, &invocation, &ledger, &arena, wrong));
        let foreign_source = Arc::new(crate::compile::room::Source::compile(
            source.owner,
            source.site.clone(),
            &source.body,
            &[],
            Default::default(),
            &std::collections::BTreeMap::new(),
            &std::collections::BTreeSet::new(),
        ));
        assert!(!proof.permits(&foreign_source, &invocation, &ledger, &arena, cell));
        drop(proof);
        assert!(arena.content_is(&old));
        assert_eq!(ledger.balance().unwrap(), before);

        // Every external root category can hide an observer behind containers.
        // The capture control is zero-price and still owns a readable alias.
        for external in [
            Owner::Session([1; 32]),
            Owner::Saved([2; 32]),
            Owner::Incoming(TaskId::MAIN),
            Owner::Ended(TaskId::MAIN),
            invocation.owner(Slot::Continuation(9)),
            invocation.owner(Slot::Departure(3)),
            invocation.owner(Slot::Capture(4)),
        ] {
            let nested = arena.tuple(vec![value], 128).unwrap();
            let root = if matches!(
                external,
                Owner::Value {
                    slot: Slot::Capture(_),
                    ..
                }
            ) {
                RootValue::Capture(cell)
            } else {
                RootValue::Value(&nested)
            };
            let prepared = ledger
                .prepare_source_ownership(&mut arena, heap, &[], &[(external, root)])
                .unwrap();
            ledger
                .commit(prepared.admit(u64::MAX).ok().unwrap())
                .unwrap();
            assert!(
                bind(&scope, &ledger, &mut arena, &physical, cell).is_none(),
                "external observer {external:?} cannot be relabeled as internal"
            );
            assert!(ledger.release(&[external], heap.pages()).unwrap());
        }
        let hidden = arena.create_list(&[value]);
        let hidden_handle = hidden.handle().unwrap();
        arena.list_set(hidden_handle, 1, hidden).unwrap();
        let external = Owner::Session([9; 32]);
        let prepared = ledger
            .prepare_source_ownership(
                &mut arena,
                heap,
                &[],
                &[(external, RootValue::Value(&hidden))],
            )
            .unwrap();
        ledger
            .commit(prepared.admit(u64::MAX).ok().unwrap())
            .unwrap();
        assert!(
            bind(&scope, &ledger, &mut arena, &physical, cell).is_none(),
            "a foreign cyclic body still observes its member"
        );
        assert!(ledger.release(&[external], heap.pages()).unwrap());
        // Unrooted cycles retain membership but cannot run a guest observer.
        assert!(bind(&scope, &ledger, &mut arena, &physical, cell).is_some());

        let external = Owner::Session([10; 32]);
        let wrong_value = Value::ObjectRef(wrong);
        let prepared = ledger
            .prepare_source_ownership(
                &mut arena,
                heap,
                &[],
                &[(external, RootValue::Value(&wrong_value))],
            )
            .unwrap();
        ledger
            .commit(prepared.admit(u64::MAX).ok().unwrap())
            .unwrap();
        let proof = bind(&scope, &ledger, &mut arena, &physical, cell).unwrap();
        let write = arena
            .prepare_cell_write(wrong, Value::Int(3))
            .unwrap()
            .unwrap();
        let edit = write.edit();
        let boundary = ledger
            .prepare_source_members_boundary(arena.registration().unwrap(), heap, &[edit], &[], &[])
            .unwrap();
        assert!(
            proof.finish(boundary).is_err(),
            "observer proof cannot authorize another member target"
        );
        assert!(matches!(arena.cell(wrong).unwrap(), Value::Int(1)));
        assert!(ledger.release(&[external], heap.pages()).unwrap());

        let proof = bind(&scope, &ledger, &mut arena, &physical, cell).unwrap();
        ledger.handoff().unwrap();
        assert!(
            !proof.permits(source, &invocation, &ledger, &arena, cell),
            "any new revision expires observer evidence"
        );
        let stale_write = arena
            .prepare_cell_write(cell, Value::Int(9))
            .unwrap()
            .unwrap();
        let boundary = ledger
            .prepare_source_members_boundary(
                arena.registration().unwrap(),
                heap,
                &[stale_write.edit()],
                &[],
                &[],
            )
            .unwrap();
        assert!(
            proof.finish(boundary).is_err(),
            "a stale observer proof cannot bind a new member exit"
        );

        // Repeated scalar shadows preserve the original cell until one exit.
        let membership = ledger.balance().unwrap().entities;
        for publish in [false, true] {
            let proof = bind(&scope, &ledger, &mut arena, &physical, cell).unwrap();
            let mut write = arena
                .prepare_cell_write(cell, Value::Int(2))
                .unwrap()
                .unwrap();
            write.stage(Value::Int(1000));
            write.stage(Value::Int(3));
            let edit = write.edit();
            let boundary = ledger
                .prepare_source_members_boundary(
                    arena.registration().unwrap(),
                    heap,
                    std::slice::from_ref(&edit),
                    &[],
                    &[],
                )
                .unwrap();
            let boundary = proof.finish(boundary).unwrap();
            if !publish {
                assert!(boundary.admit(0).is_err());
                assert!(matches!(arena.cell(cell).unwrap(), Value::Int(1)));
                assert!(arena.content_is(&old));
                continue;
            }
            boundary
                .admit(u64::MAX)
                .ok()
                .unwrap()
                .publish(&mut ledger, |arena, edits| {
                    assert_eq!(edits.len(), 1);
                    write.publish(arena, edits.into_iter().next().unwrap());
                })
                .unwrap();
            assert!(matches!(arena.cell(cell).unwrap(), Value::Int(3)));
            assert_eq!(ledger.balance().unwrap().entities, membership);
        }
        let proof = bind(&scope, &ledger, &mut arena, &physical, cell).unwrap();
        let write = arena
            .prepare_cell_write(cell, Value::Int(5))
            .unwrap()
            .unwrap();
        arena.cell_set(cell, Value::Int(4)).unwrap();
        assert!(
            !proof.permits(source, &invocation, &ledger, &arena, cell),
            "a body revision expires the private shadow evidence"
        );
        let boundary = ledger
            .prepare_source_members_boundary(
                arena.registration().unwrap(),
                heap,
                &[write.edit()],
                &[],
                &[],
            )
            .unwrap();
        let accepted = proof
            .finish(boundary)
            .unwrap()
            .admit(u64::MAX)
            .ok()
            .unwrap();
        let mut called = false;
        assert!(accepted.publish(&mut ledger, |_, _| called = true).is_err());
        assert!(!called);
        assert!(matches!(arena.cell(cell).unwrap(), Value::Int(4)));
    }
}
