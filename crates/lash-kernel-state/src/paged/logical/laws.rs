use super::heap::{Source, fresh};
use crate::paged::SealBudget;
use crate::paged::census_impl::TraceLimits;
use crate::paged::logical::*;
use crate::paged::tree::Access;
use lash_kernel_doc::TaskId;
use lash_kernel_doc::heap::{EntityId, FamilyId, HeapFault, Identity};
use std::rc::Rc;

fn id(n: u64) -> EntityId {
    EntityId {
        family: FamilyId::PRIMARY,
        ordinal: n,
    }
}
fn owner(n: u32) -> Owner {
    Owner::Value {
        task: TaskId::MAIN,
        depth: 0,
        slot: Slot::Binding(n),
    }
}
fn frame() -> Owner {
    Owner::Frame {
        task: TaskId::MAIN,
        depth: 0,
    }
}
fn value(bytes: u64, entities: &[u64], access: &Access<'_>) -> Holding {
    Holding::with_edges(
        bytes,
        entities.iter().map(|n| Identity::Object(id(*n))),
        access,
    )
    .unwrap()
}
fn admit(ledger: &mut Ledger, changes: Vec<Change>, bound: u64, access: &Access<'_>) {
    let prepared = ledger.prepare(changes, access).unwrap();
    let accepted = match prepared.admit(bound) {
        Ok(a) => a,
        Err(p) => panic!("source peak {} exceeds {bound}", p.peak()),
    };
    ledger.commit(accepted).unwrap();
}
fn budget() -> SealBudget {
    SealBudget {
        pages: u64::MAX,
        bytes: u64::MAX,
    }
}

/// LogicalHeldV1 source transfers conserve occurrences and reclaim the last
/// acyclic body across cold restore. A speculative admission publishes nothing.
#[test]
fn source_transfers_release_bodies_once_and_preserve_nested_aliases() {
    let (heap, pages) = fresh();
    let access = heap.pages().clone();
    let mut ledger = Ledger::fresh();
    let changes = vec![
        Change::Allocate {
            entity: id(0),
            body: Holding::scalar(49),
        },
        Change::Allocate {
            entity: id(1),
            body: value(64, &[0, 0], &access),
        },
        Change::Roots(vec![
            (frame(), Some(Holding::frame())),
            (owner(0), Some(value(16, &[1], &access))),
            (owner(1), Some(value(16, &[0], &access))),
        ]),
    ];
    let prepared = ledger.prepare(changes, &access).unwrap();
    assert_eq!(prepared.peak(), 209); // 49 + 64 + frame 64 + two references 32.
    let prepared = match prepared.admit(208) {
        Err(p) => p,
        Ok(_) => panic!("understated source room accepted"),
    };
    assert_eq!(ledger.balance().unwrap(), Balance::default());
    let accepted = match prepared.admit(209) {
        Ok(a) => a,
        Err(_) => panic!("exact peak refused"),
    };
    ledger.commit(accepted).unwrap();
    assert_eq!(
        ledger.balance().unwrap(),
        Balance {
            entity_local_bytes: 113,
            root_and_control_bytes: 96,
            entities: 2
        }
    );

    // The tuple has two 16-byte aliases, plus its 16-byte outer occurrence.
    admit(
        &mut ledger,
        vec![Change::Roots(vec![
            (owner(0), None),
            (owner(1), None),
            (owner(2), Some(value(48, &[0, 0], &access))),
        ])],
        209,
        &access,
    );
    assert_eq!(
        ledger.balance().unwrap(),
        Balance {
            entity_local_bytes: 49,
            root_and_control_bytes: 112,
            entities: 1
        }
    );
    let root = ledger.seal(&access, budget()).unwrap().unwrap();
    access.clear();
    let reads = pages.borrow().reads;
    let mut cold = Ledger::open(root).unwrap();
    assert_eq!(
        pages.borrow().reads,
        reads,
        "opening balances does not walk the heap"
    );
    let cold_access = Access::new(Box::new(Source(Rc::clone(&pages))));
    admit(
        &mut cold,
        vec![Change::Roots(vec![(owner(2), None)])],
        161,
        &cold_access,
    );
    assert_eq!(
        cold.balance().unwrap(),
        Balance {
            entity_local_bytes: 0,
            root_and_control_bytes: 64,
            entities: 0
        }
    );
    admit(
        &mut ledger,
        vec![Change::Roots(vec![(owner(2), None)])],
        161,
        &access,
    );
    assert_eq!(ledger.balance().unwrap(), cold.balance().unwrap());
    // Entering a closure adopts reach to its cells, rather than creating
    // another value occurrence for every physical capture slot.
    let capture = Owner::Value {
        task: TaskId::MAIN,
        depth: 0,
        slot: Slot::Capture(0),
    };
    admit(
        &mut ledger,
        vec![
            Change::Allocate {
                entity: id(2),
                body: Holding::scalar(49),
            },
            Change::Roots(vec![(owner(0), Some(value(16, &[2], &access)))]),
        ],
        129,
        &access,
    );
    admit(
        &mut ledger,
        vec![Change::Roots(vec![
            (owner(0), None),
            (capture, Some(value(0, &[2], &access))),
        ])],
        129,
        &access,
    );
    assert_eq!(ledger.balance().unwrap().total().unwrap(), 113);
    let root = ledger.seal(&access, budget()).unwrap().unwrap();
    let mut cold = Ledger::open(root).unwrap();
    assert_eq!(cold.balance().unwrap(), ledger.balance().unwrap());
    admit(
        &mut cold,
        vec![Change::Roots(vec![(capture, None)])],
        113,
        &access,
    );
    assert_eq!(cold.balance().unwrap().total().unwrap(), 64);
}

/// K-BND-003 / K-MACH-008: source-retained cycles and unreferenced ended
/// results cannot become saved floors. Normalize before an exact refusal.
#[test]
fn pressure_reclaims_cycles_and_retires_only_unreached_results_after_restore() {
    for keep_result in [false, true] {
        for restore in [false, true] {
            let (heap, pages) = fresh();
            let mut access = heap.pages().clone();
            let mut ledger = Ledger::fresh();
            let mut roots = vec![
                (frame(), Some(Holding::frame())),
                (owner(0), Some(value(16, &[0], &access))),
                (Owner::Ended(TaskId(7)), Some(value(16, &[1], &access))),
            ];
            if keep_result {
                roots.push((
                    owner(1),
                    Some(Holding::with_edges(16, [Identity::Task(TaskId(7))], &access).unwrap()),
                ));
            }
            admit(
                &mut ledger,
                vec![
                    Change::Allocate {
                        entity: id(0),
                        body: Holding::scalar(32),
                    },
                    Change::Mutate {
                        entity: id(0),
                        body: value(48, &[0], &access),
                    },
                    Change::Allocate {
                        entity: id(1),
                        body: Holding::scalar(49),
                    },
                    Change::Roots(roots),
                    Change::Roots(vec![(owner(0), None)]),
                ],
                256,
                &access,
            );
            let retained = if keep_result { 193 } else { 177 };
            assert_eq!(ledger.balance().unwrap().total().unwrap(), retained);
            if restore {
                let root = ledger.seal(&access, budget()).unwrap().unwrap();
                ledger = Ledger::open(root).unwrap();
                access = Access::new(Box::new(Source(Rc::clone(&pages))));
                assert_eq!(ledger.balance().unwrap().total().unwrap(), retained);
            }
            let limit = if keep_result { 162 } else { 81 };
            let request = || vec![Change::Roots(vec![(owner(2), Some(Holding::scalar(17)))])];
            let pending = ledger.prepare(request(), &access).unwrap();
            assert!(pending.admit(limit).is_err());
            ledger.normalize(&access, TraceLimits::default()).unwrap();
            assert_eq!(
                ledger.balance().unwrap(),
                if keep_result {
                    Balance {
                        entity_local_bytes: 49,
                        root_and_control_bytes: 96,
                        entities: 1,
                    }
                } else {
                    Balance {
                        entity_local_bytes: 0,
                        root_and_control_bytes: 64,
                        entities: 0,
                    }
                }
            );
            admit(&mut ledger, request(), limit, &access);
            assert_eq!(ledger.balance().unwrap().total().unwrap(), limit);
            if keep_result {
                admit(
                    &mut ledger,
                    vec![Change::Roots(vec![(owner(1), None)])],
                    limit,
                    &access,
                );
                ledger.normalize(&access, TraceLimits::default()).unwrap();
                assert_eq!(
                    ledger.balance().unwrap(),
                    Balance {
                        entity_local_bytes: 0,
                        root_and_control_bytes: 81,
                        entities: 0
                    }
                );
            }
        }
    }
}

/// Logical admission is tied to one unchanged source state. Host trace or
/// page failures cannot publish a partial root, and scratch cannot cross seal.
#[test]
fn failed_or_stale_source_admission_never_publishes_a_holding() {
    let (heap, _) = fresh();
    let access = heap.pages().clone();
    let mut ledger = Ledger::fresh();
    admit(
        &mut ledger,
        vec![
            Change::Allocate {
                entity: id(0),
                body: Holding::scalar(49),
            },
            Change::Roots(vec![
                (frame(), Some(Holding::frame())),
                (owner(9), Some(value(16, &[0], &access))),
            ]),
        ],
        129,
        &access,
    );
    let pending = ledger
        .prepare(
            [Change::Roots(vec![(owner(0), Some(Holding::scalar(17)))])],
            &access,
        )
        .unwrap();
    let accepted = match pending.admit(146) {
        Ok(a) => a,
        Err(_) => panic!("17-byte occurrence did not fit"),
    };
    assert!(matches!(
        ledger.normalize(
            &access,
            TraceLimits {
                work: 0,
                ..Default::default()
            }
        ),
        Err(HeapFault::Exhausted { .. })
    ));
    assert_eq!(ledger.balance().unwrap().total().unwrap(), 129);
    ledger.commit(accepted).unwrap();
    let stale = ledger
        .prepare(
            [Change::Roots(vec![(owner(1), Some(Holding::scalar(17)))])],
            &access,
        )
        .unwrap();
    let stale = match stale.admit(163) {
        Ok(a) => a,
        Err(_) => panic!("second occurrence did not fit"),
    };
    ledger.normalize(&access, TraceLimits::default()).unwrap();
    assert!(matches!(ledger.commit(stale), Err(HeapFault::Malformed(_))));
    assert_eq!(ledger.balance().unwrap().total().unwrap(), 146);
    assert!(ledger.holding(owner(1), &access).unwrap().is_none());
    let scratch = Owner::Value {
        task: TaskId::MAIN,
        depth: 0,
        slot: Slot::Scratch(2),
    };
    admit(
        &mut ledger,
        vec![Change::Roots(vec![(scratch, Some(Holding::scalar(7)))])],
        153,
        &access,
    );
    assert!(matches!(
        ledger.seal(&access, budget()),
        Err(HeapFault::Malformed(_))
    ));
    admit(
        &mut ledger,
        vec![Change::Roots(vec![(scratch, None)])],
        153,
        &access,
    );
    assert!(ledger.seal(&access, budget()).unwrap().is_some());
}
