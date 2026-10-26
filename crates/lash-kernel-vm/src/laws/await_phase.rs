//! AWAIT-PHASE: an `await` is one action, and the turns a `then yield`
//! wait takes survive a checkpoint (`K-TASK-005`, `K-TASK-006`,
//! `K-TASK-026`).
//!
//! - A run sealed while a task stands delivered, its outcome in and its
//!   turn not yet taken, and opened cold under another layout, ends as the
//!   uninterrupted run does, at every slice.
//! - A `cancel` delivered to such a wait reaches `catch` only
//!   after the turn.
//! - A delivered state at a wait that takes no turn is refused.
//! - An `await` waits where its policy waits.

use std::collections::BTreeMap;

use lash_kernel_doc::{Datum, FunctionRegistry, TaskId, parse_definition};

use lash_kernel_state::paged::{IncomingRecord, PhaseRecord, TaskStateRecord};
use lash_kernel_value::names::FunctionId;

use super::embedder::{Embedder, Setup, answer, label, wait_of};
use crate::{End, Layout, Machine, Step};

/// `policy.join(t)`: awaits a task by joining it, a body with no native.
fn policies(
    registry: &mut FunctionRegistry,
    _: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    let definition = parse_definition(
        "function policy.join(x: Any) -> Any\nkernel 1\ncharge 1\n\
         body { let v = join x return v }\n",
    )
    .unwrap();
    vec![("policy.join", registry.register(definition, None).unwrap())]
}

fn setup(slice: u64) -> Setup {
    Setup {
        slice,
        probes: Some(policies),
        ..Setup::default()
    }
}

/// A `then yield` wait nested in an expression, beside another task.
const NESTED: &str = "definition \"function other() -> Any\\nkernel 1\\nmetering statements\\nbody {  let b = perform 0(\\\"o\\\") as Any print b return b  }\"\n\nmain { let o = {x: 1} let h = spawn call other()\nlet r = [o.x, perform 0(\"m\") as Any then yield, perform 0(1) as Any then yield]\nprint \"main\" let done = join h return r }\n";

/// A `cancel` of a task at a `then yield` wait, with a third task ready.
const CANCELLED: &str = "definition \"function victim() -> Any\\nkernel 1\\nmetering statements\\nbody {  try { do perform 0(\\\"v\\\") as Any then yield } catch e { print \\\"caught\\\" }\\nreturn null  }\"\n\ndefinition \"function bystander() -> Any\\nkernel 1\\nmetering statements\\nbody {  do yield print \\\"bystander\\\" return null  }\"\n\nmain { let v = spawn call victim() let b = spawn call bystander()\ndo cancel v do yield print \"main\" let both = [v, b] do join all both return null }\n";

/// An `await` nested in an expression, by a policy that joins.
const AWAITED: &str = "definition \"function w() -> Any\\nkernel 1\\nmetering statements\\nbody {  let v = perform 0(\\\"w\\\") as Any return v  }\"\n\nmain { let h = spawn call w() let r = [1, await(h) via policy.join]\nlet again = await h via policy.join return (r, again) }\n";

#[derive(Debug, PartialEq)]
struct Trace {
    end: String,
    printed: Vec<Datum>,
    charged: u64,
}

/// How a run is driven: its slice, whether it is sealed and opened cold
/// right after each batch is delivered, and whether at every slice too.
#[derive(Clone, Copy)]
struct Drive {
    slice: u64,
    at_delivery: bool,
    at_slices: bool,
}

const STRAIGHT: Drive = Drive {
    slice: u64::MAX,
    at_delivery: false,
    at_slices: false,
};

fn end_of(end: &End) -> String {
    match end {
        End::Finished(finished) => format!("Finished({:?})", finished.result),
        other => format!("{other:?}"),
    }
}

/// Runs `text`, answering each park with every pending request delivered
/// together, in the order `order` names them. Gives the trace and how many
/// tasks stood delivered at each seal right after a delivery.
fn drive(text: &str, order: &[&str], drive: Drive) -> (Trace, Vec<usize>) {
    let mut embedder = Embedder::with(text, setup(drive.slice));
    let mut delivered = Vec::new();
    let mut hops = 0u64;
    let mut reopen = |embedder: &mut Embedder| {
        hops += 1;
        let layout = Layout(hops.wrapping_mul(0x9e37_79b9) | (hops & 1));
        embedder.reopen(layout, hops.is_multiple_of(2));
    };
    let end = loop {
        match embedder.run() {
            Step::Ended(end) => break end_of(&end),
            Step::Slice => {
                if drive.at_slices {
                    reopen(&mut embedder);
                }
            }
            Step::Parked(_) => {
                let mut batch = std::mem::take(&mut embedder.pending);
                batch.sort_by_key(|request| {
                    let name = label(request);
                    order.iter().position(|named| *named == name)
                });
                for request in &batch {
                    embedder
                        .machine
                        .deliver(wait_of(request), answer(request))
                        .unwrap();
                }
                if drive.at_delivery {
                    delivered.push(embedder.machine.delivered_tasks().len());
                    reopen(&mut embedder);
                }
            }
        }
    };
    let trace = Trace {
        end,
        printed: embedder.world.printed.clone(),
        charged: embedder.machine.meters().charged.expect("exact charge"),
    };
    (trace, delivered)
}

fn text(value: &str) -> Datum {
    Datum::Text(value.to_owned())
}

/// The uninterrupted run of `text`, and every sliced run sealed and opened
/// cold at each delivery and slice, leave the same trace; some seal holds
/// a delivered task.
fn exact(text: &str, order: &[&str]) -> Trace {
    let (reference, _) = drive(text, order, STRAIGHT);
    let mut delivered = 0;
    for slice in [u64::MAX, 7, 1, 0] {
        for (at_delivery, at_slices) in [(false, false), (true, false), (true, true)] {
            let run = Drive {
                slice,
                at_delivery,
                at_slices,
            };
            let (trace, seen) = drive(text, order, run);
            assert_eq!(
                trace, reference,
                "slice {slice} reopened {at_delivery}/{at_slices}"
            );
            delivered += seen.iter().sum::<usize>();
        }
    }
    if order.len() > 1 {
        assert!(delivered > 0, "no seal held a delivered task: {text}");
    }
    reference
}

/// AWAIT-PHASE: a `then yield` wait nested in an expression keeps its
/// operands and its turn through a checkpoint at `Delivered`.
#[test]
fn a_nested_wait_resumes_from_delivered() {
    let trace = exact(NESTED, &["m", "o"]);
    assert_eq!(trace.printed, [text("o"), text("main")]);
}

/// `K-TASK-005`, `K-TASK-017`: a task cancelled at a `then yield` wait
/// takes its turn before the raise reaches its `catch`.
#[test]
fn a_cancel_at_a_then_yield_wait_takes_its_turn() {
    let trace = exact(CANCELLED, &[]);
    assert_eq!(
        trace.printed,
        [text("bystander"), text("main"), text("caught")]
    );
}

/// CONT-GRAMMAR: a delivered state at a wait that takes no turn is
/// refused where the task's frame is decoded, at a statement's own wait
/// and at one nested in an expression.
#[test]
fn a_delivered_state_at_a_wait_without_its_turn_is_refused() {
    for text in [
        "main { let a = perform 0(\"a\") as Any return a }",
        "main { let b = [1, perform 0(\"b\") as Any] return b }",
    ] {
        let mut embedder = Embedder::with(text, setup(u64::MAX));
        assert!(matches!(embedder.run(), Step::Parked(_)));
        let request = embedder.pending.remove(0);
        embedder
            .machine
            .deliver(wait_of(&request), answer(&request))
            .unwrap();
        let wait = wait_of(&request);
        embedder
            .machine
            .tamper_task_state(
                TaskId::MAIN,
                TaskStateRecord::Resuming {
                    incoming: IncomingRecord::Outcome(wait),
                    requeue: true,
                },
            )
            .unwrap();
        let refused = match embedder.try_reopen(Layout(0x9e37_79b9 | 1), true) {
            Err(refused) => refused,
            Ok(()) => format!(
                "{:?}",
                embedder
                    .machine
                    .run(&mut embedder.world, u64::MAX)
                    .expect_err("a delivered state at a plain wait is refused")
            ),
        };
        assert!(refused.contains("disagrees"), "{text}: {refused}");
    }
}

/// `K-TASK-026`: an `await` waits where its policy waits, standing
/// called while the policy joins, and gives what the policy returns.
#[test]
fn an_await_waits_where_its_policy_waits() {
    let trace = exact(AWAITED, &["w"]);
    assert_eq!(
        trace.end,
        format!(
            "Finished({:?})",
            Datum::Tuple(vec![
                Datum::List(vec![super::embedder::int(1), text("w")]),
                text("w")
            ])
        )
    );
    let mut embedder = Embedder::with(AWAITED, setup(u64::MAX));
    assert!(matches!(embedder.run(), Step::Parked(_)));
    let phases: Vec<PhaseRecord> = embedder
        .machine
        .pending_phases()
        .into_iter()
        .map(|(_, _, phase)| phase)
        .collect();
    assert_eq!(phases, [PhaseRecord::Called]);
}
