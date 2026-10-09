//! A run parked on the await of a process handle, alone or as the next
//! pending leaf of an aggregate of them (FIG-4275), reopens from its bytes on
//! a pristine instance and ends as it would straight through.

use super::*;
use crate::testing::harness::compile_program;
use crate::{VmExecutionStart, VmInstance, VmRequest, VmResume, VmRunConfig, VmStep};

fn process_handle(name: &str) -> Expr {
    builders::record(vec![
        ("__handle__", builders::string("lash")),
        (
            "id",
            builders::string(&format!(
                "p.{}",
                lash_sansio::ProcessId::fixture(name).as_str()
            )),
        ),
    ])
}

fn handles(count: usize) -> Vec<Expr> {
    (0..count)
        .map(|index| process_handle(&format!("awaited-{index}")))
        .collect()
}

/// The awaited process's terminal: a value that names the handle, so every
/// leaf of an aggregate is told apart.
fn terminal(handle: &Value) -> Result<AbilityOutcome, ExecutionHostError> {
    Ok(AbilityOutcome::Value(Value::String(
        format!("settled {handle:?}").into(),
    )))
}

/// What a run showed its host.
#[derive(Debug, PartialEq)]
struct Run {
    /// Every await the run issued, in order, each once.
    awaits: Vec<String>,
    end: String,
}

/// Runs `program` stepwise, parking on the awaits `park` names by their
/// position among the awaits the run issues and reopening every parked run
/// from its bytes on a pristine instance. Returns the run, how many times it
/// parked, and why every declined park declined.
fn stepped(
    program: &Arc<CompiledProgram>,
    park: impl Fn(usize) -> bool,
) -> (Run, usize, Vec<String>) {
    let config = VmRunConfig::new(
        ExecutionMode::Foreground,
        ExecutionBounds::new(ExecutionBound::Unbounded, ExecutionBound::Unbounded),
    );
    let mut instance = VmInstance::pristine();
    let mut awaits = Vec::new();
    let mut parks = 0;
    let mut declined = Vec::new();
    let mut held = None::<(String, Result<AbilityOutcome, ExecutionHostError>)>;
    let mut step = instance
        .start(program.clone(), VmExecutionStart::Session, config.clone())
        .expect("the run starts");
    let end = loop {
        step = match step {
            VmStep::Suspended(suspended) => {
                let resume = match suspended.request {
                    VmRequest::Effect(op) if held.is_some() => {
                        let (issued, outcome) = held.take().expect("checked above");
                        assert_eq!(
                            format!("{op:?}"),
                            issued,
                            "a resumed run issues the await it parked on again"
                        );
                        VmResume::Effect(outcome)
                    }
                    VmRequest::Effect(AbilityOp::Await(awaited)) => {
                        let issued = format!("{:?}", AbilityOp::Await(awaited.clone()));
                        let position = awaits.len();
                        awaits.push(issued.clone());
                        if park(position) {
                            held = Some((issued, terminal(&awaited.handle)));
                            VmResume::Park
                        } else {
                            VmResume::Effect(terminal(&awaited.handle))
                        }
                    }
                    VmRequest::Effect(AbilityOp::Finish(value)) => {
                        VmResume::Effect(Ok(AbilityOutcome::Value(value)))
                    }
                    VmRequest::Effect(op) => panic!("unexpected effect {op:?}"),
                    VmRequest::CancelCheckpoint(_) => {
                        VmResume::CancelCheckpoint { cancelled: false }
                    }
                    VmRequest::Boundary => VmResume::Continue,
                    VmRequest::ParkDeclined(error) => {
                        declined.push(error.to_string());
                        VmResume::Continue
                    }
                };
                instance
                    .resume(resume)
                    .expect("the resume answers its request")
            }
            VmStep::Parked(parked) => {
                parks += 1;
                let bytes = parked
                    .continuation
                    .to_bytes()
                    .expect("a parked continuation encodes");
                instance = VmInstance::pristine();
                let continuation = instance
                    .open_continuation(&bytes)
                    .expect("a parked continuation reopens on a fresh instance");
                instance
                    .start(
                        program.clone(),
                        VmExecutionStart::Continuation(Box::new(continuation)),
                        config.clone(),
                    )
                    .expect("the continuation resumes")
            }
            VmStep::Complete(complete) => break format!("complete {:?}", complete.outcome),
            VmStep::GuestError(error) => break format!("guest error {:?}", error.failure.error),
        };
    };
    assert!(
        held.is_none(),
        "every await a run parked on was issued again"
    );
    (Run { awaits, end }, parks, declined)
}

fn awaiting(awaited: Expr) -> Arc<CompiledProgram> {
    Arc::new(compile_program(&builders::program(vec![builders::finish(
        builders::await_expr(awaited),
    )])))
}

#[test]
fn an_aggregate_await_parks_on_every_pending_handle_and_ends_as_straight_through() {
    let [a, b, c] = <[Expr; 3]>::try_from(handles(3)).expect("three handles");
    let shapes = [
        (
            "list",
            builders::list(vec![a.clone(), b.clone(), c.clone()]),
        ),
        (
            "record",
            builders::record(vec![("a", a.clone()), ("b", b.clone()), ("c", c.clone())]),
        ),
        (
            "nested",
            builders::record(vec![
                ("first", a.clone()),
                ("rest", builders::list(vec![b.clone(), c.clone()])),
            ]),
        ),
        ("lone", a.clone()),
        ("unwrapped lone", builders::unwrap(a.clone())),
        // Unwrapping a list of results is a guest error once every handle
        // has settled; a parked run meets it the same way.
        (
            "unwrapped aggregate",
            builders::unwrap(builders::list(vec![a, b, c])),
        ),
    ];
    for (shape, awaited) in shapes {
        let program = awaiting(awaited);
        let (straight, straight_parks, _) = stepped(&program, |_| false);
        assert_eq!(straight_parks, 0);
        assert_eq!(
            straight.end.starts_with("complete"),
            shape != "unwrapped aggregate",
            "{shape}: {straight:?}"
        );
        let (parked, parks, declined) = stepped(&program, |_| true);
        assert!(declined.is_empty(), "{shape}: {declined:?}");
        assert_eq!(
            parks,
            straight.awaits.len(),
            "{shape}: one park per pending handle"
        );
        assert_eq!(parked, straight, "{shape}");
        // Parking on the last handle alone carries every earlier result.
        let last = straight.awaits.len() - 1;
        let (tail, parks, _) = stepped(&program, |position| position == last);
        assert_eq!(parks, 1, "{shape}");
        assert_eq!(tail, straight, "{shape}: parked on its last handle");
    }
}

/// A continuation carries at most `VM_PARKED_AWAIT_SETTLED_LIMIT` settled
/// results: a run parked past the bound declines the park, and its host
/// answers the pending await in place.
#[test]
fn an_aggregate_await_past_the_settled_bound_declines_its_park() {
    let limit = crate::runtime::vm::VM_PARKED_AWAIT_SETTLED_LIMIT;
    let program = awaiting(builders::list(handles(limit + 2)));
    let (straight, _, _) = stepped(&program, |_| false);
    let (at_bound, parks, declined) = stepped(&program, |position| position == limit);
    assert_eq!((parks, declined.len()), (1, 0), "{declined:?}");
    assert_eq!(at_bound, straight);
    let (past_bound, parks, declined) = stepped(&program, |position| position == limit + 1);
    assert_eq!(parks, 0);
    assert_eq!(declined.len(), 1, "the park past the bound declines");
    assert!(
        declined[0].contains(&format!("{} settled handle results", limit + 1)),
        "{declined:?}"
    );
    assert_eq!(past_bound, straight);
}
