//! Source capture roles authenticate actual cells before any identity is consumed.
use crate::compile::room::{self, publication::*};
use crate::machine::source_publication::{
    BoundaryPreparation, ExternalEntry, LeafExecution, LeafFacts, Prefix,
    construction::storage::{Construction, ConstructionValue},
};
use lash_kernel_doc::{self as doc, TaskId, Value};
use lash_kernel_state::{
    arena::Arena,
    kernel_internals::logical::{Ledger, RootValue, Slot},
};
use std::sync::Arc;

pub(super) fn source_cells(
    machine: &crate::machine::KernelMachine,
    source: &Arc<room::Source>,
    identity: Arc<doc::arena::ClosureIdentity>,
    names: Vec<lash_kernel_value::Name>,
    code: doc::arena::LocalCode,
) {
    let heap = machine.source_law_heap();
    let at = room::Point {
        site: identity.site.site.clone(),
        phase: room::Phase::Commit,
    };
    let interval = source.compile_event(&at, room::ExitKind::Normal).unwrap();
    let event = interval.events().next().unwrap();
    let mut failures = Vec::new();
    for aliases in [false, true] {
        for guard in 0..8 {
            let mut arena = Arena::new();
            let left = arena.new_cell(Value::Int(7));
            let right = if aliases {
                left
            } else {
                arena.new_cell(Value::Int(7))
            };
            let foreign = arena.new_cell(Value::Int(7));
            let store = heap.logical_ledger();
            *store.borrow_mut() = Ledger::fresh();
            let mut ledger = store.borrow_mut();
            let slots = source.owner_slots().unwrap();
            let mut roots = Vec::new();
            for (name, cell) in [("left", left), ("right", right)] {
                let owner = slots
                    .root(
                        &room::Owner::Binding {
                            declared: source.site.clone(),
                            name: lash_kernel_value::Name::new(name),
                        },
                        TaskId::MAIN,
                        0,
                    )
                    .unwrap();
                let lash_kernel_state::kernel_internals::logical::Owner::Value { slot, .. } = owner
                else {
                    panic!("capture binding");
                };
                roots.push((slot, Value::ObjectRef(cell)));
            }
            roots.push((
                Slot::Binding(crate::laws::role_id(77)),
                Value::ObjectRef(foreign),
            ));
            let entries: Vec<_> = roots
                .iter()
                .map(|(slot, value)| (*slot, RootValue::Value(value)))
                .collect();
            let (invocation, entered) = ledger
                .prepare_enter(TaskId::MAIN, &mut arena, heap.pages(), &[], &entries)
                .unwrap();
            ledger
                .commit(entered.admit(u64::MAX).ok().unwrap())
                .unwrap();
            let output = slots
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
                    identity.callable.link,
                    doc::KernelVersion::One,
                    start.clone(),
                )
                .unwrap();
            let exact =
                ExactEvent::compile(source, Arc::clone(&interval), Successor::Normal).unwrap();
            let mut captures: Vec<_> = names
                .iter()
                .map(|name| if name.as_str() == "left" { left } else { right })
                .collect();
            let mut actual_names = names.clone();
            let mut actual_identity = identity.as_ref().clone();
            match guard {
                1 => captures.swap(0, 1),
                2 => actual_names.swap(0, 1),
                3 => captures[0] = foreign,
                4 => actual_identity.capture_names[0] = lash_kernel_value::Name::new("other"),
                5 => {
                    let lash_kernel_value::sites::Unit::Library(function) = source.site.unit else {
                        panic!("defining function");
                    };
                    actual_identity.callable.function = function;
                }
                6 => actual_identity.site.site = source.site.clone(),
                _ => {}
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
            let mut prepared = Construction {
                charges: None,
                value: ConstructionValue::Closure {
                    identity: Arc::new(actual_identity),
                    captures,
                    capture_names: actual_names,
                    code,
                },
                output,
                at: at.clone(),
                release: event.inputs.clone(),
                inputs: vec![],
                operands: vec![],
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
                    prefix: Prefix {
                        interval: Arc::clone(&interval),
                        facts: LeafExecution([(at.clone(), LeafFacts::default())].into()),
                        start,
                        next,
                        allowance: allowance.clone(),
                    },
                    resources: resources.facts(&allowance).unwrap(),
                },
            )
            .unwrap();
            let valid = guard == 0 || guard == 7 || (aliases && matches!(guard, 1 | 2));
            if prepared.is_some() != valid {
                failures.push(format!(
                    "capture guard {guard}, aliases {aliases}, accepted={}, expected={valid}",
                    prepared.is_some()
                ));
            }
            if let Some((event, storage)) = prepared.take() {
                if guard == 7 {
                    let accepted = event
                        .admit(&frontier, &ledger, resources, u64::MAX)
                        .unwrap()
                        .unwrap();
                    drop(ledger);
                    accepted
                        .publish_event(&mut frontier, &mut store.borrow_mut(), |arena, edits| {
                            assert!(edits.is_empty());
                            storage.batch.publish(arena);
                        })
                        .unwrap();
                    drop(prepared);
                    assert_eq!(charged, 0);
                    continue;
                }
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
                before
            );
            assert_eq!(charged, 0);
        }
    }
    assert!(
        failures.is_empty(),
        "source capture authentication failures: {failures:?}"
    );
}
