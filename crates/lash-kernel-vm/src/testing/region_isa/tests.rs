//! Deterministic test doubles until A/B/C/D are integrated. The source side
//! itemizes owners and walks reachability; the ISA side uses scalar counters.
//! Neither side can construct a production proof or qualify the cutover.
use super::*;
use lash_kernel_doc::TaskId;
use lash_kernel_state::kernel_internals::logical::Slot;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
struct Case {
    loops: u64,
    input: i64,
    alias: bool,
    cycle: bool,
    charge: u64,
    memory: u64,
    depth: u32,
    throw: Option<usize>,
    schedule: Vec<(u64, Interruption)>,
}

fn id(kind: IdentityKind, id: u64) -> Identity {
    Identity { kind, id }
}
fn scalar(value: i64) -> SemanticValue {
    SemanticValue::Scalar(value.to_be_bytes().to_vec())
}
fn binding(slot: u32) -> Owner {
    Owner::Value {
        task: TaskId(0),
        invocation: 0,
        slot: Slot::Binding(crate::laws::role_id(slot)),
    }
}

#[derive(Clone, Copy)]
struct Event {
    phase: u32,
    iteration: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct State {
    x: i64,
    next: u64,
    field: i64,
    origin: ValueOrigin,
    pending: Option<(SemanticValue, ValueOrigin)>,
    depth: u32,
    roots: Vec<(u32, Identity, u64)>,
    edges: Vec<(Identity, u32, Identity, u64)>,
    retained: Vec<(Identity, Vec<u8>)>,
    released: Vec<Identity>,
    charges: Vec<ChargeEntry>,
    debt: Vec<ChargeEntry>,
    balance: u64,
    peak: u64,
}

fn graph(case: &Case) -> State {
    let a = id(IdentityKind::Entity, 10);
    let b = id(IdentityKind::Entity, 20);
    let roots = vec![(1, a, 1), (2, if case.alias { a } else { b }, 1)];
    let edges = if case.cycle {
        vec![(a, 0, b, 1), (b, 0, a, 1)]
    } else {
        vec![]
    };
    State {
        x: case.input,
        next: 0,
        field: case.input,
        origin: ValueOrigin {
            value: id(IdentityKind::Provenance, 0),
            at: coordinate(Event {
                phase: 0,
                iteration: 0,
            }),
            kernel: b"input/0".to_vec(),
            inputs: vec![],
        },
        pending: None,
        depth: 0,
        roots,
        edges,
        retained: vec![],
        released: vec![],
        charges: vec![],
        debt: vec![],
        balance: 0,
        peak: 0,
    }
}

// A deliberately itemized source model, independent of the fake ISA's formulas.
fn source_holding(state: &mut State) {
    let previous: BTreeSet<_> = state.retained.iter().map(|(id, _)| *id).collect();
    let mut live = BTreeSet::new();
    let mut todo: Vec<_> = state.roots.iter().map(|(_, id, _)| *id).collect();
    while let Some(entity) = todo.pop() {
        if live.insert(entity) {
            todo.extend(
                state
                    .edges
                    .iter()
                    .filter(|(owner, _, _, _)| *owner == entity)
                    .map(|(_, _, target, _)| *target),
            );
        }
    }
    state.released = previous.difference(&live).copied().collect();
    state.retained = live
        .into_iter()
        .map(|id| (id, state.field.to_be_bytes().to_vec()))
        .collect();
    let mut owners = BTreeMap::from([("main".to_string(), 16_u64), ("value".into(), 8)]);
    for (slot, _, multiplicity) in &state.roots {
        owners.insert(format!("root-{slot}"), 8 * multiplicity);
    }
    for (id, _) in &state.retained {
        owners.insert(format!("entity-{}", id.id), 16);
    }
    for frame in 0..state.depth {
        owners.insert(format!("frame-{frame}"), 32);
    }
    if state.pending.is_some() {
        owners.insert("pending".into(), 8);
    }
    state.balance = owners.values().sum();
}

fn coordinate(event: Event) -> Coordinate {
    Coordinate {
        site: u64::from(event.phase),
        source_occurrence: event.iteration,
        invocation: id(
            IdentityKind::Invocation,
            if (1..=5).contains(&event.phase) {
                event.iteration + 1
            } else {
                0
            },
        ),
        occurrence: id(
            IdentityKind::Occurrence,
            event.iteration * 8 + u64::from(event.phase),
        ),
        phase: event.phase,
    }
}

fn snapshot(
    state: &State,
    event: Event,
    failure: Option<&str>,
    interruption: Option<Interruption>,
) -> PrefixSnapshot {
    let at = coordinate(event);
    let a = id(IdentityKind::Entity, 10);
    let mut values = vec![
        (binding(0), scalar(state.x), state.origin.value),
        (
            binding(1),
            SemanticValue::Reference(a),
            id(IdentityKind::Provenance, 100),
        ),
    ];
    let mut origins = vec![
        state.origin.clone(),
        ValueOrigin {
            value: id(IdentityKind::Provenance, 100),
            at: coordinate(Event {
                phase: 0,
                iteration: 0,
            }),
            kernel: b"input/1".to_vec(),
            inputs: vec![],
        },
    ];
    let mut roots = vec![
        RootFact {
            owner: Owner::Frame {
                task: TaskId(0),
                invocation: 0,
            },
            direct_bytes: 16,
            edges: vec![],
        },
        RootFact {
            owner: binding(0),
            direct_bytes: 8,
            edges: vec![],
        },
    ];
    for (role, entity, count) in &state.roots {
        roots.push(RootFact {
            owner: binding(*role),
            direct_bytes: 8,
            edges: vec![(*entity, *count)],
        });
        if *role == 2 {
            values.push((
                binding(2),
                SemanticValue::Reference(*entity),
                id(IdentityKind::Provenance, 101),
            ));
            origins.push(ValueOrigin {
                value: id(IdentityKind::Provenance, 101),
                at: coordinate(Event {
                    phase: 0,
                    iteration: 0,
                }),
                kernel: b"input/2".to_vec(),
                inputs: vec![],
            });
        }
    }
    if state.depth > 0 {
        roots.push(RootFact {
            owner: Owner::Frame {
                task: TaskId(0),
                invocation: event.iteration + 1,
            },
            direct_bytes: 32,
            edges: vec![],
        });
    }
    if let Some((value, origin)) = &state.pending {
        let owner = Owner::Value {
            task: TaskId(0),
            invocation: origin.at.invocation.id,
            slot: Slot::Operand(crate::laws::role_id(0)),
        };
        values.push((owner, value.clone(), origin.value));
        origins.push(origin.clone());
        roots.push(RootFact {
            owner,
            direct_bytes: 8,
            edges: vec![],
        });
    }
    PrefixSnapshot::observed(PrefixFacts {
        at: at.clone(),
        values,
        origins,
        roots,
        edges: state.edges.clone(),
        retained: state.retained.clone(),
        released: state.released.clone(),
        dependencies: vec![(id(IdentityKind::Lineage, 1), 3, 4), (a, 1, 2)],
        balance: state.balance,
        peak: state.peak,
        charges: state.charges.clone(),
        debt_cursor: state.charges.len() as u64,
        debt: state.debt.clone(),
        depth: state.depth,
        call_offsets: if state.depth == 0 {
            vec![]
        } else {
            vec![(at.invocation, u64::from(event.phase))]
        },
        failure: failure.map(|why| (why.into(), at)),
        stop_payload: failure
            .filter(|why| *why == "throw")
            .map(|_| scalar(state.x)),
        operands: state
            .pending
            .iter()
            .map(|(value, _)| value.clone())
            .collect(),
        pending: state.pending.as_ref().map(|(value, _)| value.clone()),
        continuation: state.next.to_be_bytes().to_vec(),
        interruption,
    })
}

fn source_oracle_fixture(case: &Case) -> Vec<PrefixSnapshot> {
    let mut state = graph(case);
    source_holding(&mut state);
    state.peak = state.balance;
    let initial_interruption = case
        .schedule
        .iter()
        .find(|(at, _)| *at == 0)
        .map(|(_, kind)| *kind);
    let injected_failure = |kind| match kind {
        Some(Interruption::BeforePrepareFailure) => Some("before prepare"),
        Some(Interruption::BeforeCommitFailure) => Some("before commit"),
        _ => None,
    };
    let initial_failure = injected_failure(initial_interruption);
    let mut out = vec![snapshot(
        &state,
        Event {
            phase: 0,
            iteration: 0,
        },
        initial_failure,
        initial_interruption,
    )];
    if initial_failure.is_some() {
        return out;
    }
    // Interpret the bounded source loop and its call body in source order.
    let mut events = Vec::new();
    for iteration in 0..case.loops {
        for phase in 1..=5 {
            events.push(Event { phase, iteration });
        }
    }
    events.push(Event {
        phase: 6,
        iteration: case.loops,
    });
    for event in events {
        let mut next = state.clone();
        let mut overflow = false;
        let units = match event.phase {
            1 => 5,
            2 => 3,
            3 | 6 => 2,
            4 => 7,
            5 => 1,
            _ => unreachable!(),
        };
        match event.phase {
            1 => {
                next.depth += 1;
                next.debt.push(ChargeEntry {
                    at: coordinate(Event {
                        phase: 5,
                        iteration: event.iteration,
                    }),
                    kind: 5,
                    units: 1,
                });
            }
            2 => {
                let wide = i128::from(next.x) + 1;
                overflow = wide > i128::from(i64::MAX);
                if !overflow {
                    next.x = wide as i64;
                    next.origin = ValueOrigin {
                        value: id(IdentityKind::Provenance, event.iteration + 1),
                        at: coordinate(event),
                        kernel: b"add".to_vec(),
                        inputs: vec![state.origin.value],
                    };
                }
            }
            3 => {
                let body = &state
                    .retained
                    .iter()
                    .find(|(entity, _)| *entity == id(IdentityKind::Entity, 10))
                    .unwrap()
                    .1;
                let read = i64::from_be_bytes(body.as_slice().try_into().unwrap());
                next.pending = Some((
                    scalar(read),
                    ValueOrigin {
                        value: id(IdentityKind::Provenance, 200 + event.iteration),
                        at: coordinate(event),
                        kernel: b"read".to_vec(),
                        inputs: vec![id(IdentityKind::Provenance, 100)],
                    },
                ));
            }
            4 => next.pending = None,
            5 => {
                next.depth -= 1;
                next.debt.pop().unwrap();
            }
            6 => {
                next.roots.pop();
            }
            _ => {}
        }
        source_holding(&mut next);
        let scratch = match event.phase {
            2 => 16,
            3 => 8,
            4 => 24,
            _ => 0,
        };
        next.peak = state.peak.max(next.balance).max(state.balance + scratch);
        let charged: u64 = state.charges.iter().map(|c| c.units).sum();
        let mut failure = if charged + units > case.charge {
            Some("charge")
        } else if next.depth > case.depth {
            Some("depth")
        } else if next.peak > case.memory {
            Some("memory")
        } else if overflow {
            Some("overflow")
        } else if case.throw == Some(out.len()) {
            Some("throw")
        } else {
            None
        };
        if failure.is_none() {
            next.charges.push(ChargeEntry {
                at: coordinate(event),
                kind: event.phase,
                units,
            });
            next.next += 1;
            state = next;
        }
        let ordinal = out.len();
        let interruption = if failure.is_none() {
            case.schedule
                .iter()
                .find(|(at, _)| *at == ordinal as u64)
                .map(|(_, kind)| *kind)
        } else {
            None
        };
        if failure.is_none() {
            failure = injected_failure(interruption);
        }
        out.push(snapshot(&state, event, failure, interruption));
        if failure.is_some() {
            break;
        }
    }
    out
}

fn isa_fixture(case: &Case, layout: IsaLayout) -> Vec<PrefixSnapshot> {
    let mut state = graph(case);
    let a = id(IdentityKind::Entity, 10);
    let b = id(IdentityKind::Entity, 20);
    let two = case.cycle || !case.alias;
    let body = case.input.to_be_bytes().to_vec();
    state.retained = if two {
        vec![(a, body.clone()), (b, body)]
    } else {
        vec![(a, body)]
    };
    state.balance = if two { 72 } else { 56 };
    state.peak = state.balance;
    let mut durable = state.clone();
    let mut out = vec![snapshot(
        &state,
        Event {
            phase: 0,
            iteration: 0,
        },
        None,
        None,
    )];
    if interrupt_fixture(case, 0, &mut durable, &mut state, &mut out[0]) {
        return out;
    }
    let mut charged = 0;
    let mut pc = 0;
    // Generic stores the micro-op index; Specialized packs the same pc. Both
    // layouts independently run the same event population, with no source call.
    let stride = match layout {
        IsaLayout::Generic => 1,
        IsaLayout::Specialized => 4,
    };
    while pc / stride <= case.loops * 5 {
        let instruction = pc / stride;
        let event = if instruction == case.loops * 5 {
            Event {
                phase: 6,
                iteration: case.loops,
            }
        } else {
            Event {
                phase: (instruction % 5 + 1) as u32,
                iteration: instruction / 5,
            }
        };
        let phase = event.phase as usize;
        let units = [0, 5, 3, 2, 7, 1, 2][phase];
        let next_depth = match phase {
            1 => 1,
            5 => 0,
            _ => state.depth,
        };
        let next_balance = match phase {
            1 => state.balance + 32,
            3 => state.balance + 8,
            4 => state.balance - 8,
            5 => state.balance - 32,
            6 => state.balance - 8 - if !case.alias && !case.cycle { 16 } else { 0 },
            _ => state.balance,
        };
        let peak = state
            .peak
            .max(next_balance)
            .max(state.balance + [0, 0, 16, 8, 24, 0, 0][phase]);
        let answer = if phase == 2 {
            state.x.checked_add(1)
        } else {
            Some(state.x)
        };
        let why = if units > case.charge.saturating_sub(charged) {
            Some("charge")
        } else if case.depth < next_depth {
            Some("depth")
        } else if case.memory < peak {
            Some("memory")
        } else if answer.is_none() {
            Some("overflow")
        } else if case.throw == Some(out.len()) {
            Some("throw")
        } else {
            None
        };
        if why.is_none() {
            state.depth = next_depth;
            state.balance = next_balance;
            state.peak = peak;
            state.released.clear();
            if phase == 1 {
                state.debt = vec![ChargeEntry {
                    at: coordinate(Event {
                        phase: 5,
                        iteration: instruction / 5,
                    }),
                    kind: 5,
                    units: 1,
                }];
            }
            if phase == 5 {
                state.debt.clear();
            }
            state.x = answer.unwrap();
            if phase == 2 {
                state.origin = ValueOrigin {
                    value: id(IdentityKind::Provenance, instruction / 5 + 1),
                    at: coordinate(event),
                    kernel: b"add".to_vec(),
                    inputs: vec![state.origin.value],
                };
            }
            if phase == 3 {
                state.pending = Some((
                    scalar(case.input),
                    ValueOrigin {
                        value: id(IdentityKind::Provenance, 200 + instruction / 5),
                        at: coordinate(event),
                        kernel: b"read".to_vec(),
                        inputs: vec![id(IdentityKind::Provenance, 100)],
                    },
                ));
            }
            if phase == 4 {
                state.pending = None;
            }
            if phase == 6 {
                state.roots.truncate(1);
                if !case.alias && !case.cycle {
                    state.retained.truncate(1);
                    state.released.push(b);
                }
            }
            state.charges.push(ChargeEntry {
                at: coordinate(event),
                kind: event.phase,
                units,
            });
            state.next = instruction + 1;
            charged += units;
        }
        let ordinal = out.len();
        out.push(snapshot(&state, event, why, None));
        if why.is_some() {
            break;
        }
        if interrupt_fixture(
            case,
            ordinal,
            &mut durable,
            &mut state,
            out.last_mut().unwrap(),
        ) {
            break;
        }
        assert_eq!(state.charges.iter().map(|c| c.units).sum::<u64>(), charged);
        pc = state.next * stride;
    }
    out
}

fn interrupt_fixture(
    case: &Case,
    ordinal: usize,
    durable: &mut State,
    state: &mut State,
    observed: &mut PrefixSnapshot,
) -> bool {
    let Some((_, interruption)) = case.schedule.iter().find(|(at, _)| *at == ordinal as u64) else {
        return false;
    };
    let before = durable.clone();
    let result = fake_publish(durable, state, Some(*interruption));
    observed.facts.as_mut().unwrap().interruption = Some(*interruption);
    match result {
        Err(fault) => {
            assert_eq!(*durable, before, "failed publication changed durable state");
            observed.facts.as_mut().unwrap().failure =
                Some((fault.into(), observed.facts.as_ref().unwrap().at.clone()));
            true
        }
        Ok(()) => {
            // The fake reopen/continuation contains only current live state;
            // the next instruction advances without executing a paid op again.
            *state = durable.clone();
            false
        }
    }
}

// The missing D transaction is a test-only double. The two injected failures
// run on opposite sides of preparation and discard all staged state.
fn fake_publish(
    durable: &mut State,
    reached: &State,
    failure: Option<Interruption>,
) -> Result<(), &'static str> {
    if failure == Some(Interruption::BeforePrepareFailure) {
        return Err("before prepare");
    }
    let prepared = reached.clone();
    if failure == Some(Interruption::BeforeCommitFailure) {
        return Err("before commit");
    }
    *durable = prepared;
    Ok(())
}

fn roomy() -> Case {
    Case {
        loops: 3,
        input: -2,
        alias: false,
        cycle: true,
        charge: u64::MAX,
        memory: u64::MAX,
        depth: u32::MAX,
        throw: None,
        schedule: vec![],
    }
}

mod authority;
mod graph;
mod laws;

mod production;
