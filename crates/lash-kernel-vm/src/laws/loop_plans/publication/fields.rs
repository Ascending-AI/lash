use super::*;
mod bodies;
use crate::compile::room;
struct Facts(std::collections::BTreeMap<room::Measure, i128>);
impl room::Facts for Facts {
    fn dependencies(&self) -> Option<&[lash_kernel_doc::arena::ArenaHandle]> {
        Some(&[])
    }
    fn measure(&self, measure: &room::Measure) -> Option<i128> {
        self.0.get(measure).copied()
    }
    fn call(
        &self,
        _: &lash_kernel_value::sites::Site,
        _: room::CallSource,
    ) -> Option<&room::CheckedRoom> {
        None
    }
    fn raised(&self) -> bool {
        false
    }
    fn resolved(
        &self,
        _: &lash_kernel_value::sites::Site,
    ) -> Option<lash_kernel_value::FunctionId> {
        None
    }
}
struct Execution<'a>(&'a Facts);
impl room::Execution for Execution<'_> {
    fn event(&mut self, _: &room::Point) -> Option<Box<dyn room::Facts + '_>> {
        Some(Box::new(Facts(self.0.0.clone())))
    }
    fn choose(&mut self, _: &lash_kernel_value::sites::Site) -> Option<bool> {
        None
    }
}

/// Equal-price field names cannot authorize each other's member journal.
pub(super) fn names() {
    bodies::final_bodies();
    private_cell_storage();
    private_member_storage();
    indexed_members();
    use crate::compile::room::{
        self, ExitKind, Measure, Owner as Selector, Phase, Point, publication::*,
    };
    use lash_kernel_doc::{self as doc, TaskId, Value};
    use lash_kernel_state::kernel_internals::logical::{Member, MemberEdit, RootValue, Slot};
    use lash_kernel_value::sites::{Site, Unit};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    let run =
        Embedder::new("main { let record = {left: 1, rght: 1} set record.left = 2 return record }");
    let (program, _) = run.program(false);
    let source = Arc::new(room::Source::compile(
        doc::constant_pool::PoolOwner::Document(program.document.identity()),
        Site::new(Unit::Main, vec![]),
        program.document.main.as_ref().unwrap(),
        &[],
        Default::default(),
        &BTreeMap::new(),
        &Default::default(),
    ));
    let at = Point {
        site: source.site.child(1),
        phase: Phase::Commit,
    };
    let interval = source.compile_event(&at, ExitKind::Normal).unwrap();
    let event = interval.events().next().unwrap();
    let heap = run.machine.source_law_heap();
    let store = heap.logical_ledger();
    let mut ledger = store.borrow_mut();
    let mut arena = lash_kernel_state::arena::Arena::new();
    let left = arena.text("left");
    let right = arena.text("rght");
    let mut nursery = doc::Nursery::unaccounted(arena.objects());
    let record = nursery.allocate(doc::NewObject::Record(vec![
        (left, Value::Int(1)),
        (right, Value::Int(1)),
    ]));
    let foreign = nursery.allocate(doc::NewObject::Record(vec![
        (left, Value::Int(1)),
        (right, Value::Int(1)),
    ]));
    arena.install(nursery.into_new()).unwrap();
    let Value::Record(handle) = record else {
        panic!("record body")
    };
    let Value::Record(foreign_handle) = foreign else {
        panic!("foreign record body")
    };
    let (invocation, entered) = ledger
        .prepare_enter(
            TaskId::MAIN,
            &mut arena,
            heap.pages(),
            &[],
            &[
                (
                    Slot::Binding(crate::laws::role_id(0)),
                    RootValue::Value(&record),
                ),
                (
                    Slot::Binding(crate::laws::role_id(1)),
                    RootValue::Value(&foreign),
                ),
            ],
        )
        .unwrap();
    ledger
        .commit(entered.admit(u64::MAX).ok().unwrap())
        .unwrap();
    let slots = source.owner_slots().unwrap();
    let values: Vec<_> = event
        .inputs
        .iter()
        .map(|selector| {
            let Selector::Operand(site) = selector else {
                panic!("evaluated member operands")
            };
            (
                slots.root(selector, TaskId::MAIN, invocation.id()).unwrap(),
                if site == &at.site.child(1) {
                    Value::Int(2)
                } else {
                    record
                },
            )
        })
        .collect();
    let outputs: Vec<_> = values
        .iter()
        .map(|(owner, value)| (*owner, RootValue::Value(value)))
        .collect();
    let held = ledger
        .prepare_ownership(&mut arena, heap.pages(), &[], &outputs)
        .unwrap();
    ledger.commit(held.admit(u64::MAX).ok().unwrap()).unwrap();
    let inputs: Vec<_> = values.iter().map(|(owner, _)| *owner).collect();
    let mut facts = BTreeMap::new();
    for (selector, (owner, _)) in event.inputs.iter().zip(&values) {
        facts.insert(
            Measure::Local(selector.clone()),
            i128::from(
                ledger
                    .holding(*owner, heap.pages())
                    .unwrap()
                    .unwrap()
                    .bytes(),
            ),
        );
    }
    facts.insert(Measure::MemberDelta(at.site.clone()), 0);
    facts.insert(
        Measure::Release(event.inputs.clone()),
        i128::from(ledger.release_cost(&inputs, heap.pages()).unwrap()),
    );
    let facts = Facts(facts);
    let position = Position {
        at: at.clone(),
        unpaid: Some(at.clone()),
        recovery: [1; 32],
        occurrence: Occurrence {
            task: TaskId::MAIN,
            invocation: invocation.id(),
            loops: vec![],
            action: None,
        },
    };
    drop(ledger);
    let ledger = store.borrow();
    let frontier = source
        .frontier(
            &ledger,
            heap,
            &invocation,
            program.link,
            doc::KernelVersion::One,
            position.clone(),
        )
        .unwrap();
    let next = Position {
        unpaid: Some(source.next_after_commit(&at.site).unwrap()),
        ..position
    };
    let allowance = crate::admission::Allowance {
        occurrences: vec![],
        before: 0,
        units: 0,
        close: None,
        strict: false,
        depth: 0,
        safe_point: 7,
    };
    let exact = ExactEvent::compile(&source, Arc::clone(&interval), Successor::Normal).unwrap();
    let operands: Vec<_> = values.iter().map(|(_, value)| *value).collect();
    for (target, name, next_value, valid) in [
        (handle, "left", Value::Int(2), true),
        (handle, "rght", Value::Int(2), false),
        (foreign_handle, "left", Value::Int(2), false),
        (handle, "left", Value::Int(3), false),
    ] {
        let mut charged = 0;
        let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
        let resources = crate::admission::SourceResources {
            charged: &mut charged,
            counters: &mut counters,
            bound: 10,
            slice: 10,
            depth: 0,
            limit: 10,
            safe_point: 7,
        };
        let edit = MemberEdit {
            handle: target,
            member: Member::Field(Arc::from(name)),
            next: Some(next_value),
        };
        let evaluated = exact.bind_paid_external(
            &frontier,
            &ledger,
            &arena,
            heap,
            &mut Execution(&facts),
            ExactRoots {
                inputs: inputs.clone(),
                ..Default::default()
            },
            next.clone(),
            allowance.clone(),
            resources.facts(&allowance).unwrap(),
            ExternalFacts {
                operands: &operands,
                member: Some(&edit),
                ..Default::default()
            },
        );
        assert_eq!(
            evaluated.is_ok(),
            valid,
            "only the admitted code field and actual target operand may be edited: {name} {target:?}"
        );
        if valid {
            let transaction = ledger
                .prepare_source_members_boundary(
                    arena.registration().unwrap(),
                    heap,
                    &[edit],
                    &inputs,
                    &[],
                )
                .unwrap();
            evaluated
                .unwrap()
                .finish_external(&frontier, &ledger, transaction)
                .unwrap();
        }
        assert_eq!(charged, 0);
        assert!(matches!(
            arena.table_get(handle, &left).unwrap(),
            Some(Value::Int(1))
        ));
        assert!(matches!(
            arena.table_get(handle, &right).unwrap(),
            Some(Value::Int(1))
        ));
    }
}

/// A folded list/table overwrite installs its final edges once and retains
/// the original table key and insertion order. Abandonment exposes no write.
fn private_member_storage() {
    use lash_kernel_doc::{TaskId, Value};
    use lash_kernel_state::{
        arena::Arena,
        kernel_internals::logical::{Ledger, RootValue, Slot},
    };
    let run = Embedder::new("main { return null }");
    let heap = run.machine.source_law_heap();
    let mut ledger = Ledger::fresh();
    let mut arena = Arena::new();
    let child = arena.create_list(&[Value::Int(101)]);
    let list = arena.create_list(&[child]);
    let key = arena.text("original");
    let neighbor = arena.text("neighbor");
    let table = arena
        .create_map([(key, child), (neighbor, Value::Int(77))].into_iter())
        .unwrap();
    let list_handle = list.handle().unwrap();
    let table_handle = table.handle().unwrap();
    let (invocation, entered) = ledger
        .prepare_enter(
            TaskId::MAIN,
            &mut arena,
            heap.pages(),
            &[],
            &[
                (
                    Slot::Binding(crate::laws::role_id(0)),
                    RootValue::Value(&list),
                ),
                (
                    Slot::Binding(crate::laws::role_id(1)),
                    RootValue::Value(&table),
                ),
            ],
        )
        .unwrap();
    ledger
        .commit(entered.admit(u64::MAX).ok().unwrap())
        .unwrap();
    let before = ledger.balance().unwrap();
    let list_version = arena.content_version(list_handle).unwrap();
    let table_version = arena.content_version(table_handle).unwrap();
    for publish in [false, true] {
        let mut list_write = arena
            .prepare_list_write(list_handle, 0, Value::Int(2))
            .unwrap()
            .unwrap();
        list_write.stage(child);
        list_write.stage(Value::Int(3));
        assert!(list_write.value().same(&Value::Int(3)));
        let mut table_write = arena
            .prepare_table_member(table_handle, key, Some(Value::Int(4)))
            .unwrap()
            .unwrap();
        assert!(table_write.stage(child));
        assert!(table_write.stage(Value::Int(5)));
        assert!(table_write.value().unwrap().same(&Value::Int(5)));
        assert!(arena.item(list_handle, 0).unwrap().unwrap().same(&child));
        assert!(
            arena
                .table_get(table_handle, &key)
                .unwrap()
                .unwrap()
                .same(&child)
        );
        let edits = [list_write.edit(), table_write.edit()];
        let boundary = ledger
            .prepare_source_members_boundary(arena.registration().unwrap(), heap, &edits, &[], &[])
            .unwrap();
        if publish {
            boundary
                .admit(u64::MAX)
                .ok()
                .unwrap()
                .publish(&mut ledger, |arena, edits| {
                    let [list_edit, table_edit] = edits.as_slice() else {
                        panic!("two final members")
                    };
                    list_write.publish(arena, list_edit.clone());
                    table_write.publish(arena, table_edit.clone());
                })
                .unwrap();
        } else {
            drop(boundary);
            assert_eq!(ledger.balance().unwrap(), before);
            assert!(arena.content_is(&list_version));
            assert!(arena.content_is(&table_version));
        }
    }
    assert!(
        arena
            .item(list_handle, 0)
            .unwrap()
            .unwrap()
            .same(&Value::Int(3))
    );
    assert!(
        arena
            .table_get(table_handle, &key)
            .unwrap()
            .unwrap()
            .same(&Value::Int(5))
    );
    let entries = arena.entries(table_handle).unwrap();
    assert!(entries[0].same(&key));
    assert!(entries[2].same(&neighbor));
    assert!(entries[3].same(&Value::Int(77)));
    assert!(!arena.content_is(&list_version));
    assert!(!arena.content_is(&table_version));
    assert!(
        ledger
            .entity_holding(
                arena.ledger_key(child.handle().unwrap()).unwrap(),
                heap.pages()
            )
            .unwrap()
            .is_none()
    );
    for slot in [
        Slot::Binding(crate::laws::role_id(0)),
        Slot::Binding(crate::laws::role_id(1)),
    ] {
        assert!(
            ledger
                .holding(invocation.owner(slot), heap.pages())
                .unwrap()
                .is_some()
        );
    }
}

/// Private repeated writes stay detached until one boundary publishes the
/// original cell, alias/capture owners and canonical retained cyclic graph.
fn private_cell_storage() {
    use lash_kernel_doc::{TaskId, Value};
    use lash_kernel_state::{
        arena::Arena,
        kernel_internals::logical::{Dependencies, Ledger, RootValue, Slot},
    };

    let run = Embedder::new("main { return null }");
    let heap = run.machine.source_law_heap();
    let mut ledger = Ledger::fresh();
    let mut arena = Arena::new();
    let cell = arena.new_cell(Value::Int(7));
    let second = arena.new_cell(Value::Int(13));
    let foreign = arena.new_cell(Value::Int(41));
    let variable = Value::ObjectRef(cell);
    let second_value = Value::ObjectRef(second);
    let foreign_value = Value::ObjectRef(foreign);
    let cycle = arena.create_list(&[Value::Int(9)]);
    let cycle_handle = cycle.handle().unwrap();
    arena.list_set(cycle_handle, 0, cycle).unwrap();
    let (invocation, entered) = ledger
        .prepare_enter(
            TaskId::MAIN,
            &mut arena,
            heap.pages(),
            &[],
            &[
                (
                    Slot::Binding(crate::laws::role_id(0)),
                    RootValue::Value(&variable),
                ),
                (
                    Slot::Binding(crate::laws::role_id(1)),
                    RootValue::Value(&variable),
                ),
                (
                    Slot::Binding(crate::laws::role_id(2)),
                    RootValue::Value(&second_value),
                ),
                (
                    Slot::Binding(crate::laws::role_id(3)),
                    RootValue::Value(&foreign_value),
                ),
                (
                    Slot::Capture(crate::laws::role_id(0)),
                    RootValue::Capture(cell),
                ),
            ],
        )
        .unwrap();
    ledger
        .commit(entered.admit(u64::MAX).ok().unwrap())
        .unwrap();
    ledger
        .prepare_source_boundary(
            arena.registration().unwrap(),
            heap,
            &[],
            &[(
                invocation.owner(Slot::Operand(crate::laws::role_id(0))),
                RootValue::Value(&cycle),
            )],
        )
        .unwrap()
        .admit(u64::MAX)
        .ok()
        .unwrap()
        .publish(&mut ledger, |_, edits| {
            assert!(edits.is_empty());
        })
        .unwrap();
    let before = ledger.balance().unwrap();
    let version = arena.content_version(cell).unwrap();
    let inputs = [invocation.owner(Slot::Operand(crate::laws::role_id(0)))];

    // Abandonment and memory refusal both discard the private writes. No
    // source increment is replayed because none has reached the arena yet.
    for refuse in [false, true] {
        let mut write = arena
            .prepare_cell_write(cell, Value::Int(8))
            .unwrap()
            .unwrap();
        write.stage(Value::Int(19));
        write.stage(cycle);
        assert!(write.value().same(&cycle));
        let boundary = ledger
            .prepare_source_members_boundary(
                arena.registration().unwrap(),
                heap,
                &[write.edit()],
                &inputs,
                &[],
            )
            .unwrap();
        if refuse {
            assert!(boundary.admit(0).is_err());
        } else {
            drop(boundary);
        }
        assert!(arena.cell(cell).unwrap().same(&Value::Int(7)));
        assert!(arena.content_is(&version));
        assert_eq!(ledger.balance().unwrap(), before);
    }

    // Both original member provenance and discarded foreign reads belong to
    // entry. Re-projecting a changed equal-price body at exit cannot hide it.
    for stale in [foreign, cell] {
        let dependencies = Dependencies::capture(&ledger, &arena, &[stale]).unwrap();
        let write = arena.prepare_cell_write(cell, cycle).unwrap().unwrap();
        arena.cell_set(stale, Value::Int(42)).unwrap();
        let boundary = ledger
            .prepare_source_members_boundary(
                arena.registration().unwrap(),
                heap,
                &[write.edit()],
                &inputs,
                &[],
            )
            .unwrap()
            .bind_dependencies(dependencies)
            .unwrap();
        let mut edited = false;
        assert!(
            boundary
                .admit(u64::MAX)
                .ok()
                .unwrap()
                .publish(&mut ledger, |_, _| {
                    edited = true;
                })
                .is_err()
        );
        assert!(!edited);
        let expected = if stale == cell { 42 } else { 7 };
        assert!(arena.cell(cell).unwrap().same(&Value::Int(expected)));
        assert_eq!(ledger.balance().unwrap(), before);
        arena.cell_set(cell, Value::Int(7)).unwrap();
    }

    let mut first = arena
        .prepare_cell_write(cell, Value::Int(8))
        .unwrap()
        .unwrap();
    first.stage(Value::Int(19));
    first.stage(cycle);
    let second_write = arena.prepare_cell_write(second, variable).unwrap().unwrap();
    let edits = [first.edit(), second_write.edit()];
    // Independent snapshots of the same body must never silently overwrite
    // one another. The private kernel submits its single final reservation.
    assert!(
        ledger
            .prepare_source_members_boundary(
                arena.registration().unwrap(),
                heap,
                &[edits[0].clone(), edits[0].clone()],
                &[],
                &[],
            )
            .is_err()
    );
    let dependencies =
        Dependencies::capture(&ledger, &arena, &[cell, second, cycle_handle]).unwrap();
    let boundary = ledger
        .prepare_source_members_boundary(arena.registration().unwrap(), heap, &edits, &inputs, &[])
        .unwrap()
        .bind_dependencies(dependencies)
        .unwrap();
    boundary
        .admit(u64::MAX)
        .ok()
        .unwrap()
        .publish(&mut ledger, |arena, edits| {
            let [first_edit, second_edit] = edits.as_slice() else {
                panic!("two final cell edits")
            };
            first.publish(arena, first_edit.clone());
            second_write.publish(arena, second_edit.clone());
        })
        .unwrap();
    assert!(arena.cell(cell).unwrap().same(&cycle));
    assert!(arena.cell(second).unwrap().same(&variable));
    assert!(arena.cell(foreign).unwrap().same(&Value::Int(42)));
    assert!(!arena.content_is(&version));
    assert!(ledger.holding(inputs[0], heap.pages()).unwrap().is_none());
    for slot in [
        Slot::Binding(crate::laws::role_id(0)),
        Slot::Binding(crate::laws::role_id(1)),
        Slot::Capture(crate::laws::role_id(0)),
    ] {
        let held = ledger
            .holding(invocation.owner(slot), heap.pages())
            .unwrap()
            .unwrap();
        let identity = lash_kernel_doc::heap::Identity::Object(arena.ledger_key(cell).unwrap());
        let mut edges = Vec::new();
        held.visit_edges(heap.pages(), &mut |target, count| {
            edges.push((target, count));
            Ok(())
        })
        .unwrap();
        assert_eq!(edges, vec![(identity, 1)]);
    }

    // Removing the last external edge does not erase the itemized self-cycle.
    // The unrelated cell remains a retained control.
    let write = arena
        .prepare_cell_write(cell, Value::Int(23))
        .unwrap()
        .unwrap();
    ledger
        .prepare_source_members_boundary(
            arena.registration().unwrap(),
            heap,
            &[write.edit()],
            &[],
            &[],
        )
        .unwrap()
        .admit(u64::MAX)
        .ok()
        .unwrap()
        .publish(&mut ledger, |arena, edits| {
            write.publish(arena, edits.into_iter().next().unwrap());
        })
        .unwrap();
    assert!(
        ledger
            .entity_holding(arena.ledger_key(cycle_handle).unwrap(), heap.pages())
            .unwrap()
            .is_some()
    );
    assert!(
        ledger
            .holding(
                invocation.owner(Slot::Binding(crate::laws::role_id(3))),
                heap.pages()
            )
            .unwrap()
            .is_some()
    );
    assert!(arena.item(cycle_handle, 0).unwrap().unwrap().same(&cycle));
}

/// The actual index and set-presence RHS authenticate an edit independently
/// of equal holding sizes or the final graph delta.
fn indexed_members() {
    use crate::compile::room::{self, ExitKind, Measure, Phase, Point, publication::*};
    use lash_kernel_doc::{self as doc, TaskId, Value};
    use lash_kernel_state::kernel_internals::logical::{Member, MemberEdit, RootValue, Slot};
    use lash_kernel_value::sites::{Site, Unit};
    use std::{collections::BTreeMap, sync::Arc};
    for kind in 0..5 {
        let text = match kind {
            0 => "main { let values = [1, 1] set values[0] = 2 return values }",
            1 => "main { let values = map{0: 1, 1: 1} set values[0] = 2 return values }",
            2 => "main { let values = set{1} set values[0] = true return values }",
            3 => "main { let values = set{0, 1} set values[0] = false return values }",
            _ => "main { let values = [1, 1] remove values[0] return values }",
        };
        let run = Embedder::new(text);
        let (program, _) = run.program(false);
        let source = Arc::new(room::Source::compile(
            doc::constant_pool::PoolOwner::Document(program.document.identity()),
            Site::new(Unit::Main, vec![]),
            program.document.main.as_ref().unwrap(),
            &[],
            Default::default(),
            &BTreeMap::new(),
            &Default::default(),
        ));
        let heap = run.machine.source_law_heap();
        let store = heap.logical_ledger();
        let mut ledger = store.borrow_mut();
        let mut arena = lash_kernel_state::arena::Arena::new();
        let object = match kind {
            0 | 4 => doc::NewObject::List(vec![Value::Int(1), Value::Int(1)]),
            1 => doc::NewObject::Map(vec![
                (Value::Int(0), Value::Int(1)),
                (Value::Int(1), Value::Int(1)),
            ]),
            2 => doc::NewObject::Set(vec![Value::Int(1)]),
            _ => doc::NewObject::Set(vec![Value::Int(0), Value::Int(1)]),
        };
        let mut nursery = doc::Nursery::unaccounted(arena.objects());
        let target = nursery.allocate(object.clone());
        let foreign = nursery.allocate(object);
        arena.install(nursery.into_new()).unwrap();
        let (invocation, entered) = ledger
            .prepare_enter(
                TaskId::MAIN,
                &mut arena,
                heap.pages(),
                &[],
                &[
                    (
                        Slot::Binding(crate::laws::role_id(0)),
                        RootValue::Value(&target),
                    ),
                    (
                        Slot::Binding(crate::laws::role_id(1)),
                        RootValue::Value(&foreign),
                    ),
                ],
            )
            .unwrap();
        ledger
            .commit(entered.admit(u64::MAX).ok().unwrap())
            .unwrap();
        let at = Point {
            site: source.site.child(1),
            phase: Phase::Commit,
        };
        let interval = source.compile_event(&at, ExitKind::Normal).unwrap();
        let event = interval.events().next().unwrap();
        let mut operands = vec![target, Value::Int(0)];
        if kind < 4 {
            operands.push(if kind >= 2 {
                Value::Bool(kind == 2)
            } else {
                Value::Int(2)
            });
        }
        assert_eq!(event.inputs.len(), operands.len());
        let slots = source.owner_slots().unwrap();
        let inputs: Vec<_> = event
            .inputs
            .iter()
            .map(|owner| slots.root(owner, TaskId::MAIN, invocation.id()).unwrap())
            .collect();
        let roots: Vec<_> = inputs
            .iter()
            .zip(&operands)
            .map(|(owner, value)| (*owner, RootValue::Value(value)))
            .collect();
        let held = ledger
            .prepare_ownership(&mut arena, heap.pages(), &[], &roots)
            .unwrap();
        ledger.commit(held.admit(u64::MAX).ok().unwrap()).unwrap();
        let edit = MemberEdit {
            handle: target.handle().unwrap(),
            member: if matches!(kind, 0 | 4) {
                Member::List(0)
            } else {
                Member::Table(Value::Int(0))
            },
            next: match kind {
                0 | 1 => Some(Value::Int(2)),
                2 => Some(Value::Absent),
                _ => None,
            },
        };
        let member = ledger
            .prepare_source_members_boundary(
                arena.registration().unwrap(),
                heap,
                std::slice::from_ref(&edit),
                &[],
                &[],
            )
            .unwrap();
        let delta = i128::from(member.balance().unwrap().total().unwrap())
            - i128::from(ledger.balance().unwrap().total().unwrap());
        let released = member.release_cost(&inputs, heap.pages()).unwrap();
        drop(member);
        let mut measures = BTreeMap::new();
        for (selector, owner) in event.inputs.iter().zip(&inputs) {
            measures.insert(
                Measure::Local(selector.clone()),
                i128::from(
                    ledger
                        .holding(*owner, heap.pages())
                        .unwrap()
                        .unwrap()
                        .bytes(),
                ),
            );
        }
        measures.insert(Measure::MemberDelta(at.site.clone()), delta);
        measures.insert(Measure::Release(event.inputs.clone()), i128::from(released));
        let start = Position {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: [1; 32],
            occurrence: Occurrence {
                task: TaskId::MAIN,
                invocation: invocation.id(),
                loops: vec![],
                action: None,
            },
        };
        let next = Position {
            unpaid: Some(source.next_after_commit(&at.site).unwrap()),
            ..start.clone()
        };
        drop(ledger);
        let ledger = store.borrow();
        let frontier = source
            .frontier(
                &ledger,
                heap,
                &invocation,
                program.link,
                doc::KernelVersion::One,
                start,
            )
            .unwrap();
        let exact = ExactEvent::compile(&source, Arc::clone(&interval), Successor::Normal).unwrap();
        let mut wrong_key = edit.clone();
        wrong_key.member = if matches!(kind, 0 | 4) {
            Member::List(1)
        } else {
            Member::Table(Value::Int(1))
        };
        let mut wrong_target = edit.clone();
        wrong_target.handle = foreign.handle().unwrap();
        let mut wrong_value = edit.clone();
        wrong_value.next = Some(Value::Int(3));
        for (actual, valid) in [
            (&edit, true),
            (&wrong_key, false),
            (&wrong_target, false),
            (&wrong_value, false),
        ] {
            let facts = Facts(measures.clone());
            let mut execution = Execution(&facts);
            let mut charged = 7;
            let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
            let resources = crate::admission::SourceResources {
                charged: &mut charged,
                counters: &mut counters,
                bound: 20,
                slice: 20,
                depth: 1,
                limit: 10,
                safe_point: 7,
            };
            let allowance = crate::admission::Allowance {
                occurrences: vec![],
                before: 0,
                units: 0,
                close: None,
                strict: false,
                depth: 0,
                safe_point: 7,
            };
            let bound = exact.bind_paid_external(
                &frontier,
                &ledger,
                &arena,
                heap,
                &mut execution,
                ExactRoots {
                    inputs: inputs.clone(),
                    ..Default::default()
                },
                next.clone(),
                allowance.clone(),
                resources.facts(&allowance).unwrap(),
                ExternalFacts {
                    operands: &operands,
                    member: Some(actual),
                    ..Default::default()
                },
            );
            assert_eq!(
                bound.is_ok(),
                valid,
                "actual member roles kind={kind}, edit={actual:?}"
            );
            if valid {
                let transaction = ledger
                    .prepare_source_members_boundary(
                        arena.registration().unwrap(),
                        heap,
                        std::slice::from_ref(actual),
                        &inputs,
                        &[],
                    )
                    .unwrap();
                bound
                    .unwrap()
                    .finish_external(&frontier, &ledger, transaction)
                    .unwrap();
            }
            assert_eq!(charged, 7);
        }
    }
}
