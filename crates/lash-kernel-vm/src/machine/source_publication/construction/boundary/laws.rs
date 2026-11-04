/// Source allocation order survives reversed binding order, interior retirement,
/// immutable nesting and canonical duplicate-key normalization. This executes the
/// detached boundary API directly; common source acceptance is core-owned.
pub(super) fn prepared_graphs(machine: &crate::machine::KernelMachine) {
    use super as boundary;
    use lash_kernel_doc::{NewObject, Nursery, TaskId, Value};
    use lash_kernel_state::{
        arena::Arena,
        kernel_internals::logical::{Dependencies, Ledger, Owner, Slot},
    };
    let heap = machine.source_law_heap();
    let mut arena = Arena::new();
    let mut ledger = Ledger::fresh();
    let owner = |slot| Owner::Value {
        task: TaskId::MAIN,
        invocation: 0,
        slot: Slot::Binding(slot),
    };
    for attempt in 0..4 {
        let dependencies = Dependencies::capture(&ledger, &arena, &[]).unwrap();
        let before = (arena.allocations(), arena.next_provisional());
        let mut nursery = Nursery::unaccounted(arena.objects());
        let first = nursery.allocate(NewObject::List(vec![Value::Int(7)]));
        let retired = nursery.allocate(NewObject::List(vec![]));
        let outer = nursery.allocate(NewObject::List(vec![first]));
        let tuple = nursery.allocate(NewObject::Tuple(vec![outer, Value::Int(17)]));
        let key = nursery.text("same");
        let map = nursery.allocate(NewObject::Map(vec![
            (key, Value::Int(1)),
            (key, Value::Int(2)),
        ]));
        let set = nursery.allocate(NewObject::Set(vec![
            Value::Int(1),
            Value::Int(1),
            Value::Int(2),
        ]));
        let record = nursery.allocate(NewObject::Record(vec![
            (key, Value::Int(1)),
            (key, Value::Int(3)),
        ]));
        // The last allocated mutable body is the first binding live-out.
        let outputs = [
            (owner(0), record),
            (owner(1), tuple),
            (owner(2), map),
            (owner(3), set),
        ];
        let new = nursery.into_new();
        let graph = boundary::prepare_collections(&mut arena, heap, new, &outputs, 128)
            .unwrap()
            .unwrap();
        assert_eq!(
            graph
                .proof
                .entities()
                .iter()
                .map(|entity| entity.entity.ordinal)
                .collect::<Vec<_>>(),
            [0, 1, 2, 3, 4, 5]
        );
        if attempt == 0 {
            drop(graph);
        } else {
            let (transaction, storage) = graph
                .into_boundary(&ledger, heap, &[], dependencies)
                .unwrap();
            assert_eq!(
                storage.members.len(),
                6,
                "one canonical body summary for every source allocation"
            );
            if attempt == 1 {
                assert!(transaction.admit(0).is_err());
            } else {
                let accepted = transaction
                    .admit(u64::MAX)
                    .unwrap_or_else(|_| panic!("unbounded construction fits"));
                if attempt == 2 {
                    ledger.handoff().unwrap();
                }
                let result = accepted.publish(&mut ledger, |arena, edits| {
                    assert!(edits.is_empty());
                    storage.batch.publish(arena);
                });
                if attempt == 2 {
                    assert!(result.is_err());
                } else {
                    result.unwrap();
                }
            }
        }
        if attempt < 3 {
            assert_eq!((arena.allocations(), arena.next_provisional()), before);
            for value in [first, retired, outer, tuple, map, set, record] {
                assert!(
                    arena.body(value.handle().unwrap()).is_err(),
                    "unaccepted storage is invisible"
                );
            }
            assert_eq!(ledger.balance().unwrap().total().unwrap(), 0);
        } else {
            assert_eq!(arena.next_provisional().ordinal, 6);
            for (value, ordinal) in [
                (first, 0),
                (retired, 1),
                (outer, 2),
                (map, 3),
                (set, 4),
                (record, 5),
            ] {
                assert_eq!(
                    arena
                        .facts(value.handle().unwrap())
                        .unwrap()
                        .ledger_entity()
                        .unwrap()
                        .ordinal,
                    ordinal
                );
            }
            assert!(arena.sequence(outer.handle().unwrap()).unwrap()[0].same(&first));
            assert!(arena.sequence(tuple.handle().unwrap()).unwrap()[0].same(&outer));
            let map_entries = arena.entries(map.handle().unwrap()).unwrap();
            assert_eq!(map_entries.len(), 2);
            assert!(map_entries[0].same(&key));
            assert!(map_entries[1].same(&Value::Int(2)));
            let record_entries = arena.entries(record.handle().unwrap()).unwrap();
            assert_eq!(record_entries.len(), 2);
            assert!(record_entries[1].same(&Value::Int(3)));
            assert_eq!(arena.entries(set.handle().unwrap()).unwrap().len(), 4);
            assert_eq!(
                ledger.balance().unwrap().entities,
                5,
                "retired interior storage consumes an ordinal but no source membership"
            );
        }
    }
}

pub(super) fn prepared_literals_and_captures(
    machine: &crate::machine::KernelMachine,
    identity: std::sync::Arc<lash_kernel_doc::arena::ClosureIdentity>,
    code: lash_kernel_doc::arena::LocalCode,
) {
    use super as boundary;
    use lash_kernel_doc::{self as doc, TaskId, Value};
    use lash_kernel_state::{
        arena::Arena,
        kernel_internals::logical::{Dependencies, Ledger, Owner, RootValue, Slot},
    };
    use std::sync::Arc;
    let heap = machine.source_law_heap();
    let link = identity.callable.link;
    let function = identity.callable.function;
    let owner = |slot| Owner::Value {
        task: TaskId::MAIN,
        invocation: 0,
        slot: Slot::Binding(slot),
    };
    let mut arena = Arena::new();
    let mut ledger = Ledger::fresh();
    let cell = arena.new_cell(Value::Int(7));
    let declaring = Value::ObjectRef(cell);
    ledger
        .prepare_source_boundary(
            arena.registration().unwrap(),
            heap,
            &[],
            &[(owner(0), RootValue::Value(&declaring))],
        )
        .unwrap()
        .admit(u64::MAX)
        .unwrap_or_else(|_| panic!("capture fits"))
        .publish(&mut ledger, |_, _| {})
        .unwrap();
    let before = arena.next_provisional();
    let dependencies = Dependencies::capture(&ledger, &arena, &[cell]).unwrap();
    let mut graph = boundary::prepare_closure(
        &mut arena,
        heap,
        Arc::clone(&identity),
        vec![cell],
        code,
        owner(1),
    )
    .unwrap();
    let abandoned = graph.outputs[0].value;
    graph
        .registration
        .arena()
        .cell_set(cell, Value::Int(8))
        .unwrap();
    assert!(
        graph
            .into_boundary(&ledger, heap, &[owner(0)], dependencies)
            .is_err(),
        "a changed capture invalidates the entire detached proof before admission"
    );
    assert_eq!(arena.next_provisional(), before);
    assert!(arena.body(abandoned.handle().unwrap()).is_err());
    let dependencies = Dependencies::capture(&ledger, &arena, &[cell]).unwrap();
    let graph =
        boundary::prepare_closure(&mut arena, heap, identity, vec![cell], code, owner(1)).unwrap();
    let closure = graph.outputs[0].value;
    let (transaction, storage) = graph
        .into_boundary(&ledger, heap, &[owner(0)], dependencies)
        .unwrap();
    transaction
        .admit(u64::MAX)
        .unwrap_or_else(|_| panic!("capture fits"))
        .publish(&mut ledger, |arena, _| storage.batch.publish(arena))
        .unwrap();
    assert!(ledger.holding(owner(0), heap.pages()).unwrap().is_none());
    arena.release(&mut |visit| visit(&closure));
    assert!(
        arena.cell(cell).unwrap().same(&Value::Int(8)),
        "the closure keeps its capture after the declaring root leaves"
    );
    assert_eq!(arena.next_provisional().ordinal, before.ordinal + 1);
    for (slot, literal) in [
        doc::Literal::Int(doc::Integer::parse("18446744073709551616").unwrap()),
        doc::Literal::Text(
            "literal text longer than the small text representation must keep its exact contents"
                .into(),
        ),
        doc::Literal::Bytes(doc::Bytes::new(vec![1, 2, 3])),
        doc::Literal::Function(function),
    ]
    .into_iter()
    .enumerate()
    {
        let dependencies = Dependencies::capture(&ledger, &arena, &[]).unwrap();
        let graph =
            boundary::prepare_literal(&mut arena, heap, &literal, link, owner(slot as u32 + 2))
                .unwrap();
        assert!(
            graph.proof.entities().is_empty(),
            "an immutable literal consumes no entity ordinal"
        );
        let value = graph.outputs[0].value;
        let holding = graph.outputs[0].holding.bytes();
        let (transaction, storage) = graph
            .into_boundary(&ledger, heap, &[], dependencies)
            .unwrap();
        assert_eq!(storage.outputs.len(), 1);
        transaction
            .admit(u64::MAX)
            .unwrap_or_else(|_| panic!("literal fits"))
            .publish(&mut ledger, |arena, _| storage.batch.publish(arena))
            .unwrap();
        assert_eq!(
            ledger
                .holding(owner(slot as u32 + 2), heap.pages())
                .unwrap()
                .unwrap()
                .bytes(),
            holding
        );
        match literal {
            doc::Literal::Int(expected) => assert_eq!(
                arena.semantics().integer(&value).unwrap().unwrap(),
                expected
            ),
            doc::Literal::Text(expected) => {
                assert_eq!(arena.semantics().text(&value).unwrap(), expected.as_str())
            }
            doc::Literal::Bytes(expected) => assert_eq!(
                arena.semantics().bytes(value.handle().unwrap()).unwrap(),
                expected.as_slice()
            ),
            doc::Literal::Function(expected) => assert_eq!(
                arena.callable(value.handle().unwrap()).unwrap().function,
                expected
            ),
            _ => unreachable!(),
        }
    }
    assert_eq!(arena.next_provisional().ordinal, before.ordinal + 1);
}

#[test]
fn resident_graph_projection_commutes_at_the_budget_edge() {
    let mut failures = Vec::new();
    for shape in 0..5 {
        let projected = graph_execution(true, shape, u64::MAX);
        let resident = graph_execution(false, shape, u64::MAX);
        if resident != projected {
            failures.push(format!("unbounded graph {shape}"));
        }
        let peak = projected.0;
        assert!(peak > 0);
        for bound in [peak - 1, peak] {
            let projected = graph_execution(true, shape, bound);
            let resident = graph_execution(false, shape, bound);
            if resident != projected {
                failures.push(format!("graph {shape}, bound {bound}"));
            }
            if resident.1 != (bound == peak) {
                failures.push(format!("admission edge {shape}, bound {bound}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "commuting projection failures: {failures:?}"
    );
    let run = crate::laws::embedder::Embedder::new(
        "main { let make = fn(x) { let f = fn() { return x } return f } return apply make(7) }",
    );
    let exe = &run.machine.exe;
    let (index, target) = (0..exe.code_count())
        .map(|index| (index, exe.code(crate::compile::CodeId(index as u32))))
        .find(|(_, code)| code.created.is_some() && code.captures.len() == 1)
        .unwrap();
    let lash_kernel_value::sites::Unit::Library(function) = target.site.unit else {
        panic!("a real source closure");
    };
    let link = run.program(false).0.link;
    let identity = std::sync::Arc::new(lash_kernel_doc::arena::ClosureIdentity {
        capture_names: target.capture_names.clone(),
        callable: lash_kernel_value::names::CallableId { link, function },
        site: lash_kernel_doc::linked_site::LinkedSite {
            link,
            site: target.created.as_ref().unwrap().clone(),
        },
    });
    prepared_graphs(&run.machine);
    prepared_literals_and_captures(
        &run.machine,
        identity,
        lash_kernel_doc::arena::LocalCode {
            executable: exe.local(),
            code: index as u32,
        },
    );
}

type HoldingView = (u64, Vec<(lash_kernel_doc::heap::Identity, u64)>);
#[derive(Debug, PartialEq, Eq)]
struct GraphView {
    balance: lash_kernel_state::kernel_internals::logical::Balance,
    roots: Vec<HoldingView>,
    bodies: Vec<Option<HoldingView>>,
    values: Vec<Vec<u8>>,
}
fn holding_view(
    holding: &lash_kernel_state::kernel_internals::logical::Holding,
    heap: &lash_kernel_state::kernel_internals::PagedHeap,
) -> HoldingView {
    let mut edges = Vec::new();
    holding
        .visit_edges(heap.pages(), &mut |identity, count| {
            edges.push((identity, count));
            Ok(())
        })
        .unwrap();
    (holding.bytes(), edges)
}
fn graph_execution(projected: bool, shape: u8, bound: u64) -> (u64, bool, Option<GraphView>) {
    use lash_kernel_doc::{self as doc, NewObject, Nursery, Value};
    use lash_kernel_state::{
        arena::Arena,
        kernel_internals::{
            logical::{Dependencies, Ledger, Owner, RootValue, Slot, VirtualEntity},
            stored_pages,
        },
    };
    let machine = crate::laws::embedder::Embedder::new("main { return null }");
    let heap = machine.machine.source_law_heap();
    let mut arena = Arena::new();
    let mut ledger = Ledger::fresh();
    let owner = |slot| Owner::Value {
        task: doc::TaskId::MAIN,
        invocation: 0,
        slot: Slot::Binding(slot),
    };
    let mut nursery = Nursery::unaccounted(arena.objects());
    let alias = nursery.allocate(NewObject::List(vec![Value::Int(9)]));
    let new = nursery.into_new();
    arena.install(new).unwrap();
    ledger
        .prepare_source_boundary(
            arena.registration().unwrap(),
            heap,
            &[],
            &[(owner(99), RootValue::Value(&alias))],
        )
        .unwrap()
        .admit(u64::MAX)
        .ok()
        .unwrap()
        .publish(&mut ledger, |_, _| {})
        .unwrap();
    let page = crate::machine::project::project(&mut arena, heap.pages(), &alias).unwrap();
    ledger.rekey(&arena.take_rekeyed(), heap.pages()).unwrap();
    let alias = if shape == 4 {
        let tuple = stored_pages::tuple(heap.pages(), vec![page; 256]).unwrap();
        crate::machine::project::admit(
            &mut arena,
            heap.pages(),
            &doc::heap::PageValue::Tuple(tuple),
        )
        .unwrap()
    } else {
        alias
    };
    let before = (
        arena.allocations(),
        arena.next_provisional(),
        ledger.balance().unwrap(),
    );
    let first_ordinal = arena.next_provisional().ordinal;
    let mut nursery = Nursery::unaccounted(arena.objects());
    let first = nursery.allocate(NewObject::List(vec![Value::Int(7)]));
    let second = nursery.allocate(NewObject::List(vec![first, first, alias]));
    let tuple = nursery.allocate(NewObject::Tuple(vec![
        second,
        first,
        alias,
        Value::Task(doc::TaskId(7)),
        Value::TaskRef(doc::TaskId(7)),
    ]));
    let error = nursery.allocate(NewObject::Error {
        kind: Value::Text(doc::arena::ShortText::new("kind").unwrap()),
        message: Value::Text(doc::arena::ShortText::new("message").unwrap()),
        data: tuple,
    });
    let map = nursery.allocate(NewObject::Map(vec![
        (Value::Int(1), first),
        (Value::Float(doc::Float::new(1.0)), second),
    ]));
    let set = nursery.allocate(NewObject::Set(vec![
        Value::Int(1),
        Value::Float(doc::Float::new(1.0)),
        Value::Int(2),
    ]));
    let name = nursery.text("a field name longer than a short text fits");
    let record = nursery.allocate(NewObject::Record(vec![(name, tuple)]));
    let mut new = nursery.into_new();
    if shape == 1 {
        new[0].1 = NewObject::List(vec![second]);
        new[1].1 = NewObject::List(vec![alias]);
    }
    if shape == 2 {
        new[0].1 = NewObject::List(vec![first]);
    }
    let values = [if shape == 3 { error } else { tuple }, map, set, record];
    let outputs: Vec<_> = values
        .iter()
        .enumerate()
        .map(|(index, value)| (owner(index as u32), *value))
        .collect();
    let allocations: Vec<_> = new
        .iter()
        .map(|(handle, object)| object.naming()(*handle))
        .collect();
    let mutable: Vec<_> = new
        .iter()
        .filter_map(|(handle, object)| {
            matches!(
                object,
                NewObject::List(_) | NewObject::Map(_) | NewObject::Set(_) | NewObject::Record(_)
            )
            .then_some(*handle)
        })
        .collect();
    let (transaction, batch) = if projected {
        let mut installation = arena.prepare_install(new).unwrap();
        for (index, handle) in mutable.iter().enumerate() {
            installation
                .bind_entity(
                    *handle,
                    doc::heap::EntityId {
                        family: doc::heap::FamilyId::PROVISIONAL,
                        ordinal: first_ordinal + index as u64,
                    },
                )
                .unwrap();
        }
        // The independent oracle is the previous canonical page transition,
        // including normalization and every allocation's immutable validation.
        for value in &allocations {
            crate::machine::project::project_prepared(&mut installation, heap.pages(), value)
                .unwrap();
        }
        let entities: Vec<_> = mutable
            .iter()
            .enumerate()
            .map(|(index, handle)| {
                let object = installation.collection(*handle).unwrap();
                VirtualEntity {
                    entity: doc::heap::EntityId {
                        family: doc::heap::FamilyId::PROVISIONAL,
                        ordinal: first_ordinal + index as u64,
                    },
                    body: projected_body(&mut installation, heap, object),
                }
            })
            .collect();
        let pages: Vec<_> = values
            .iter()
            .map(|value| {
                crate::machine::project::project_prepared(&mut installation, heap.pages(), value)
                    .unwrap()
            })
            .collect();
        let roots: Vec<_> = pages
            .iter()
            .enumerate()
            .map(|(index, page)| (owner(index as u32), RootValue::Page(page)))
            .collect();
        let (registration, batch) = installation.into_parts();
        let (transaction, _) = ledger
            .prepare_construction_boundary(registration, heap, &[owner(99)], &roots, &entities)
            .unwrap();
        (transaction, batch)
    } else {
        let dependencies = Dependencies::capture(&ledger, &arena, &[]).unwrap();
        let graph = super::prepare_collections(&mut arena, heap, new, &outputs, 128)
            .unwrap()
            .unwrap();
        let (transaction, storage) = graph
            .into_boundary(&ledger, heap, &[owner(99)], dependencies)
            .unwrap();
        (transaction, storage.batch)
    };
    let peak = transaction.peak();
    let admitted = transaction.admit(bound);
    if admitted.is_err() {
        drop(admitted);
        drop(batch);
        assert_eq!(
            (
                arena.allocations(),
                arena.next_provisional(),
                ledger.balance().unwrap()
            ),
            before,
            "refusal publishes no storage, grants or roots"
        );
        for value in &allocations {
            assert!(
                arena.body(value.handle().unwrap()).is_err(),
                "refusal installed a detached body"
            );
        }
        return (peak, false, None);
    }
    let accepted = admitted.ok().unwrap();
    accepted
        .publish(&mut ledger, |arena, _| batch.publish(arena))
        .unwrap();
    let mut encoded = Vec::new();
    for value in values.iter().chain(&allocations) {
        let page = crate::machine::project::project(&mut arena, heap.pages(), value).unwrap();
        encoded.push(lash_kernel_value::tokens::encode_body(&page));
    }
    // Project direct members too: identity traversal stops at each mutable
    // occurrence, so cycles and shared objects remain distinct.
    for handle in &mutable {
        let values = match arena.body(*handle).unwrap() {
            doc::arena::Body::List(_) => arena.sequence(*handle).unwrap().to_vec(),
            _ => arena.entries(*handle).unwrap().to_vec(),
        };
        for value in values {
            encoded.push(lash_kernel_value::tokens::encode_body(
                &crate::machine::project::project(&mut arena, heap.pages(), &value).unwrap(),
            ));
        }
    }
    ledger.rekey(&arena.take_rekeyed(), heap.pages()).unwrap();
    let roots = (0..4)
        .map(|index| {
            holding_view(
                &ledger.holding(owner(index), heap.pages()).unwrap().unwrap(),
                heap,
            )
        })
        .collect();
    let bodies = mutable
        .iter()
        .map(|handle| {
            ledger
                .entity_holding(
                    arena.facts(*handle).unwrap().ledger_entity().unwrap(),
                    heap.pages(),
                )
                .unwrap()
                .map(|holding| holding_view(&holding, heap))
        })
        .collect();
    (
        peak,
        true,
        Some(GraphView {
            balance: ledger.balance().unwrap(),
            roots,
            bodies,
            values: encoded,
        }),
    )
}
fn projected_body(
    installation: &mut lash_kernel_state::arena::PreparedInstallation<'_>,
    heap: &lash_kernel_state::kernel_internals::PagedHeap,
    object: lash_kernel_doc::NewObject,
) -> lash_kernel_state::kernel_internals::logical::VirtualBody {
    use lash_kernel_doc::NewObject;
    use lash_kernel_state::kernel_internals::logical::VirtualBody as B;
    let mut project = |value| {
        crate::machine::project::project_prepared(installation, heap.pages(), &value).unwrap()
    };
    match object {
        NewObject::List(values) => B::List(values.into_iter().map(&mut project).collect()),
        NewObject::Set(values) => B::Set(values.into_iter().map(&mut project).collect()),
        NewObject::Map(values) => B::Map(
            values
                .into_iter()
                .map(|(key, value)| (project(key), project(value)))
                .collect(),
        ),
        NewObject::Record(values) => B::Record(
            values
                .into_iter()
                .map(|(key, value)| (project(key), project(value)))
                .collect(),
        ),
        _ => panic!("the oracle has a mutable collection"),
    }
}

#[test]
fn detached_proofs_reject_stale_foreign_and_substituted_batches() {
    use lash_kernel_doc::{NewObject, Nursery, TaskId, Value};
    use lash_kernel_state::{
        arena::Arena,
        kernel_internals::logical::{Dependencies, Ledger, Owner, RootValue, Slot},
    };
    let machine = crate::laws::embedder::Embedder::new("main { return null }");
    let heap = machine.machine.source_law_heap();
    let mut arena = Arena::new();
    let ledger = Ledger::fresh();
    let mut failures = Vec::new();
    let output = Owner::Value {
        task: TaskId::MAIN,
        invocation: 0,
        slot: Slot::Binding(0),
    };
    let make = |arena: &mut Arena| {
        let mut nursery = Nursery::unaccounted(arena.objects());
        let value = nursery.allocate(NewObject::List(vec![Value::Int(7)]));
        (value, nursery.into_new())
    };
    let before = (
        arena.allocations(),
        arena.next_provisional(),
        ledger.balance().unwrap(),
    );
    let (_value, new) = make(&mut arena);
    let (proof, old_batch) = {
        let old =
            super::prepare_collections(&mut arena, heap, new, &[(output, Value::Int(7))], 128)
                .unwrap()
                .unwrap();
        let proof = old.outputs[0].proof.clone();
        let old_batch = old.batch;
        drop(old.registration);
        (proof, old_batch)
    };
    assert!(
        arena.body(_value.handle().unwrap()).is_err(),
        "abandonment installed a body"
    );
    // Re-entering the same arena does not refresh a proof from an abandoned
    // registration. This call reaches Ledger::project_root directly.
    {
        let mut registration = arena.registration().unwrap();
        if ledger
            .prepare_source_ownership(
                registration.arena(),
                heap,
                &[],
                &[(output, RootValue::Prepared(&proof))],
            )
            .is_ok()
        {
            failures.push("stale proof reached project_root");
        }
    }
    let mut foreign = Arena::new();
    {
        let mut registration = foreign.registration().unwrap();
        if ledger
            .prepare_source_ownership(
                registration.arena(),
                heap,
                &[],
                &[(output, RootValue::Prepared(&proof))],
            )
            .is_ok()
        {
            failures.push("foreign proof reached project_root");
        }
    }
    let (value, new) = make(&mut arena);
    let dependencies = Dependencies::capture(&ledger, &arena, &[]).unwrap();
    let mut graph = super::prepare_collections(&mut arena, heap, new, &[(output, value)], 128)
        .unwrap()
        .unwrap();
    graph.batch = old_batch;
    if graph
        .into_boundary(&ledger, heap, &[], dependencies)
        .is_ok()
    {
        failures.push("substituted batch");
    }
    assert_eq!(
        (
            arena.allocations(),
            arena.next_provisional(),
            ledger.balance().unwrap()
        ),
        before
    );
    assert!(
        arena.body(value.handle().unwrap()).is_err(),
        "substitution installed a body"
    );
    // Even a dead immutable allocation must pass the depth check.
    let mut nursery = Nursery::unaccounted(arena.objects());
    let inner = nursery.allocate(NewObject::Tuple(vec![Value::Int(1)]));
    nursery.allocate(NewObject::Tuple(vec![inner]));
    let new = nursery.into_new();
    if super::prepare_collections(&mut arena, heap, new, &[(output, Value::Int(7))], 1)
        .unwrap()
        .is_some()
    {
        failures.push("invalid dead allocation");
    }
    assert_eq!(
        (
            arena.allocations(),
            arena.next_provisional(),
            ledger.balance().unwrap()
        ),
        before
    );
    assert!(
        arena.body(inner.handle().unwrap()).is_err(),
        "depth refusal installed an immutable"
    );
    let mut nursery = Nursery::unaccounted(arena.objects());
    let mut shared = Value::Int(1);
    for _ in 0..64 {
        shared = nursery.allocate(NewObject::Tuple(vec![shared, shared]));
    }
    let new = nursery.into_new();
    if super::prepare_collections(&mut arena, heap, new, &[(output, Value::Int(7))], 128).is_ok() {
        failures.push("immutable occurrence overflow in a dead allocation");
    }
    assert_eq!(
        (
            arena.allocations(),
            arena.next_provisional(),
            ledger.balance().unwrap()
        ),
        before
    );
    assert!(
        arena.body(shared.handle().unwrap()).is_err(),
        "overflow refusal installed an immutable"
    );
    assert!(failures.is_empty(), "detached proof refusals: {failures:?}");
}

#[test]
fn a_cold_shared_empty_tuple_keeps_its_constructor_facts() {
    use lash_kernel_doc::{self as doc, Value};
    use lash_kernel_state::{
        arena::Arena,
        kernel_internals::logical::{Owner, Slot},
    };
    let run = crate::laws::embedder::Embedder::new("main { return null }");
    let heap = run.machine.source_law_heap();
    let mut arena = Arena::new();
    let (empty, installation) = arena.prepare_empty_tuple().unwrap();
    installation.publish();
    let page = crate::machine::project::project(&mut arena, heap.pages(), &empty).unwrap();
    assert!(matches!(page, doc::heap::PageValue::Tuple(tuple) if tuple.summary.len == 0));
    let chilled = arena.chill(empty.handle().unwrap()).unwrap().unwrap();
    let output = Owner::Value {
        task: doc::TaskId::MAIN,
        invocation: 0,
        slot: Slot::Binding(0),
    };
    let before = (arena.allocations(), arena.next_provisional());
    let mut graph =
        super::prepare_collection(&mut arena, heap, doc::NewObject::Tuple(vec![]), output, 128)
            .unwrap()
            .unwrap();
    assert!(graph.outputs[0].value.same(&empty));
    assert!(
        matches!(graph.outputs[0].proof.collection(), Some(doc::NewObject::Tuple(values)) if values.is_empty()),
        "cold empty tuple lost its exact constructor facts"
    );
    assert_eq!(
        graph.outputs[0].holding.bytes(),
        doc::LogicalHeldV1::REFERENCE
    );
    assert!(graph.proof.entities().is_empty());
    assert!(matches!(graph.outputs[0].value, Value::Tuple(_)));
    assert!(
        graph
            .registration
            .arena()
            .needs_decode(empty.handle().unwrap())
            .unwrap(),
        "proof construction must not warm the cold tuple"
    );
    drop(graph);
    assert_eq!((arena.allocations(), arena.next_provisional()), before);
    arena.thaw(empty.handle().unwrap(), chilled).unwrap();
}
