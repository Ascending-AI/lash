/// A same-price recipe cannot substitute another source value, kind or role.
pub(super) fn source_certificates(
    machine: &crate::machine::KernelMachine,
    source: &std::sync::Arc<crate::compile::room::Source>,
    link: lash_kernel_value::content_ids::LinkId,
    expression: &str,
) -> Vec<String> {
    let mut failures = Vec::new();
    use crate::compile::room::{self, publication::*};
    use crate::machine::source_publication::{
        BoundaryPreparation, ExternalEntry, LeafExecution, LeafFacts, Prefix,
        construction::storage::{Construction, ConstructionValue},
    };
    use lash_kernel_doc::{self as doc, TaskId, Value};
    use lash_kernel_state::{
        arena::Arena,
        kernel_internals::logical::{Ledger, RootValue},
    };
    use std::sync::Arc;
    let at = room::Point {
        site: if expression == "cell" {
            source.site.child(0)
        } else {
            source.site.child(0).child(0)
        },
        phase: room::Phase::Commit,
    };
    let interval = source.compile_event(&at, room::ExitKind::Normal).unwrap();
    let event = interval.events().next().unwrap();
    let heap = machine.source_law_heap();
    for guard in 0..8 {
        let mut arena = Arena::new();
        let store = heap.logical_ledger();
        *store.borrow_mut() = Ledger::fresh();
        let mut ledger = store.borrow_mut();
        let (invocation, entered) = ledger
            .prepare_enter(TaskId::MAIN, &mut arena, heap.pages(), &[], &[])
            .unwrap();
        ledger
            .commit(entered.admit(u64::MAX).ok().unwrap())
            .unwrap();
        let slots = source.owner_slots().unwrap();
        let mut inputs: Vec<_> = event
            .inputs
            .iter()
            .map(|owner| slots.root(owner, TaskId::MAIN, invocation.id()).unwrap())
            .collect();
        let values = if expression == "cell" {
            vec![Value::Int(1)]
        } else {
            vec![Value::Int(1), Value::Int(2)]
        };
        let roots: Vec<_> = inputs
            .iter()
            .zip(&values)
            .map(|(owner, value)| (*owner, RootValue::Value(value)))
            .collect();
        let preparation = ledger
            .prepare_source_ownership(&mut arena, heap, &[], &roots)
            .unwrap();
        ledger
            .commit(preparation.admit(u64::MAX).ok().unwrap())
            .unwrap();
        let mut output = slots
            .root(
                event.output.as_ref().unwrap(),
                TaskId::MAIN,
                invocation.id(),
            )
            .unwrap();
        let start = Position {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: [0; 32],
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
        let mut frontier = source
            .frontier(
                &ledger,
                heap,
                &invocation,
                link,
                doc::KernelVersion::One,
                start.clone(),
            )
            .unwrap();
        let exact = ExactEvent::compile(source, Arc::clone(&interval), Successor::Normal).unwrap();
        let (literal, object) = match expression {
            "cell" => (None, None),
            "[1, 2]" => (None, Some(doc::NewObject::List(values.to_vec()))),
            "(1, 2)" => (None, Some(doc::NewObject::Tuple(values.to_vec()))),
            "map{1: 2}" => (
                None,
                Some(doc::NewObject::Map(vec![(values[0], values[1])])),
            ),
            "set{1, 2}" => (None, Some(doc::NewObject::Set(values.to_vec()))),
            "{left: 1, rght: 2}" => (
                None,
                Some(doc::NewObject::Record(vec![
                    (arena.text("left"), values[0]),
                    (arena.text("rght"), values[1]),
                ])),
            ),
            _ => (
                Some(doc::Literal::Text(
                    if guard == 1 {
                        "b deferred literal larger than the inline text capacity"
                    } else {
                        "a deferred literal larger than the inline text capacity"
                    }
                    .into(),
                )),
                None,
            ),
        };
        let mut operands = if literal.is_some() {
            vec![]
        } else {
            values.to_vec()
        };
        let mut object = object;
        // Each refusal has the same source price as the valid preparation.
        if guard == 1 && literal.is_none() {
            operands[0] = Value::Int(9);
        }
        if guard == 2 && inputs.len() > 1 {
            inputs.swap(0, 1);
        }
        if guard == 3 && !inputs.is_empty() {
            inputs.pop();
        }
        if guard == 4 {
            output = invocation.owner(
                lash_kernel_state::kernel_internals::logical::Slot::Argument(crate::laws::role_id(
                    77,
                )),
            );
        }
        if guard == 5 {
            if let Some(doc::NewObject::List(values)) = object.clone() {
                object = Some(doc::NewObject::Set(values));
            } else if expression == "{left: 1, rght: 2}" {
                object = Some(doc::NewObject::Record(vec![
                    (arena.text("rght"), values[0]),
                    (arena.text("left"), values[1]),
                ]));
            } else if literal.is_none() {
                operands.reverse();
            }
        }
        if guard == 6 && !operands.is_empty() {
            operands.pop();
        }
        let allowance = crate::admission::Allowance {
            occurrences: vec![],
            before: 0,
            units: 0,
            close: None,
            strict: false,
            depth: 0,
            safe_point: 0,
        };
        let mut charged = 0;
        let mut counters = crate::machine::tasks::Occurrences::for_source_law(0);
        let resources = crate::admission::SourceResources {
            charged: &mut charged,
            counters: &mut counters,
            bound: 1000,
            slice: 1000,
            depth: 0,
            limit: 10,
            safe_point: 0,
        };
        let before = (
            arena.allocations(),
            arena.next_provisional(),
            ledger.balance().unwrap(),
        );
        let value = if expression == "cell" {
            ConstructionValue::Cell(Value::Int(if guard == 5 { 9 } else { 1 }))
        } else if let Some(literal) = &literal {
            ConstructionValue::Literal { literal, link }
        } else {
            ConstructionValue::Collection(object.unwrap())
        };
        let prefix = Prefix {
            interval: Arc::clone(&interval),
            facts: LeafExecution([(at.clone(), LeafFacts::default())].into()),
            start: start.clone(),
            next,
            allowance: allowance.clone(),
        };
        let mut prepared = Construction {
            charges: None,
            value,
            output,
            at: at.clone(),
            release: event.inputs.clone(),
            inputs,
            operands,
            depth_limit: 128,
            heap,
        }
        .prepare(
            &mut arena,
            ExternalEntry {
                event: &exact,
                frontier: &frontier,
                ledger: &ledger,
                heap,
                prefix,
                resources: resources.facts(&allowance).unwrap(),
            },
        )
        .unwrap();
        let valid = guard == 0
            || guard == 7
            || (expression == "cell" && guard == 2)
            || (literal.is_some() && matches!(guard, 2 | 3 | 5 | 6));
        if prepared.is_some() != valid {
            failures.push(format!(
                "{expression}, guard {guard}: accepted={}, expected={valid}",
                prepared.is_some()
            ));
        }
        if let Some((event, storage)) = prepared.take() {
            if guard == 7 {
                let event = event
                    .admit(&frontier, &ledger, resources, u64::MAX)
                    .unwrap()
                    .unwrap();
                drop(ledger);
                event
                    .publish_event(&mut frontier, &mut store.borrow_mut(), |arena, edits| {
                        assert!(edits.is_empty());
                        storage.batch.publish(arena);
                    })
                    .unwrap();
                assert!(
                    store
                        .borrow()
                        .holding(output, heap.pages())
                        .unwrap()
                        .is_some()
                );
                drop(prepared);
                assert_eq!(charged, 0);
                continue;
            }
            // Tight memory must still decline without consuming the batch.
            assert!(
                event
                    .admit(&frontier, &ledger, resources, 0)
                    .unwrap()
                    .is_none()
            );
        }
        drop(prepared);
        assert_eq!(
            (
                arena.allocations(),
                arena.next_provisional(),
                ledger.balance().unwrap()
            ),
            before,
            "abandoned constructor consumes no identity or membership"
        );
        assert_eq!(charged, 0);
    }
    failures
}
