//! The executable aggregate oracle (FIG-3395).
//!
//! Everything this repository knew about async aggregates was asked of the VM
//! in isolation: `lashlang::execute` against a hand-written `ExecutionHost`
//! that answers in one `perform` call and decides its own settlement order.
//! That harness cannot state the questions the aggregate landing is about,
//! because the order leaves settle in is the host's answer rather than the
//! runtime's observation, and nothing durable is written. The test262 slice
//! cannot serve either: it answers only `Finish`/`Print`, bars
//! `flags: [async]`, and none of its vendored probes is a Promise test.
//!
//! So the cases below are authored async aggregates run end to end — provider
//! cell, RLM bridge, real tool dispatch, real batch scheduler — against a
//! **journaled** store tier, with a host whose leaves settle in an order the
//! *test* controls. The order is decided by construction rather than by
//! timing: a leaf either parks on a completion key this test resolves, or
//! blocks inside its attempt until this test releases it. No case sleeps, and
//! no case asserts on a duration.
//!
//! Three pins the FIG-3397 landing re-pointed rather than discovered:
//!
//! * [`sqlite_a_rejected_promise_all_resumes_at_its_first_consumed_rejection`]
//!   retires ADR 0062 deviation 15: a rejected `Promise.all` resumes at the
//!   first rejection its consumer takes, while a held sibling is still in
//!   flight as a loser (ADR 0099 §10 L2).
//! * [`sqlite_a_terminal_leaf_settles_ahead_of_a_held_source_first_leaf`]
//!   pins ADR 0099 §5's settlement semantics: group children commit in durable
//!   commit order, so a held source-first leaf does not block a later sibling's
//!   terminal and the first-settled selection reports the later leaf's
//!   rejection.
//! * [`the_standalone_list_batch_still_selects_the_first_written_rejection`]
//!   pins the ruling that Lashlang-native aggregates wait for every result and
//!   report their first *written* rejection (ADR 0099 §10 L7).
//!
//! The compile-time aggregate path (`Instruction::ResourceOperationBatch`)
//! has had no authored spelling since ADR 0096 retired the second dialect, so
//! it is stated separately, at the IR, rather than through the bridge.
//!
//! ## Registering another tier
//!
//! Every case body takes a [`JournaledTier`] and asserts nothing about which
//! one it got, so PostgreSQL registers the same list by adding a
//! `JournaledTier` constructor and one `#[tokio::test]` wrapper per case.

use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// The theatre: what the host was asked to do, and who settles when
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// One aggregate case, run end to end
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Shapes: what the host is asked to do for each spelling of one aggregate
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Selection: which rejection a rejected aggregate reports
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Leaves that are not ordinary tool calls
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// The compile-time aggregate paths, stated at the IR
// ---------------------------------------------------------------------------
//
// `Instruction::ResourceOperationBatch` is formed by the compiler from
// `Expr::Await` over a list or a record. ADR 0096 retired the dialect that
// spelled those, so it is not reachable from an authored cell and cannot be
// stated through the bridge above. It is stated here, at the IR, because
// FIG-3397 changed it and a landing that only re-points the bridge cases would
// move it silently. The written-order ruling is ADR 0099 §10 L7.

/// A host that records every batch it is handed and answers each leaf with a
/// rejection. It runs its leaves in reverse, so a selection that followed the
/// order leaves ran in would report leaf 1; the Lashlang-native aggregates ask
/// it for every result and report in written order (ADR 0099 §10 L7).
struct AllResultsHost {
    /// The batch sizes the VM asked for, in order.
    batches: StdMutex<Vec<usize>>,
    /// The consumer mode each batch asked for.
    consumers: StdMutex<Vec<lashlang::AggregateConsumer>>,
    calls: AtomicUsize,
}

impl AllResultsHost {
    fn new() -> Self {
        Self {
            batches: StdMutex::new(Vec::new()),
            consumers: StdMutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        }
    }
}

impl lashlang::ExecutionHost for AllResultsHost {
    async fn perform(
        &self,
        op: lashlang::AbilityOp,
    ) -> std::result::Result<lashlang::AbilityOutcome, lashlang::ExecutionHostError> {
        match op {
            lashlang::AbilityOp::ResourceOperationBatch(batch) => {
                self.batches.lock_recover().push(batch.leaves.len());
                self.consumers.lock_recover().push(batch.consumer);
                self.calls.fetch_add(batch.leaves.len(), Ordering::SeqCst);
                let mut results = vec![None; batch.leaves.len()];
                for index in (0..batch.leaves.len()).rev() {
                    results[index] = Some(lashlang::ResourceOperationOutcome::Error(
                        lashlang::ExecutionHostError::new(format!("leaf-{index} rejected")),
                    ));
                }
                Ok(lashlang::AbilityOutcome::ResourceOperationBatch(
                    lashlang::ResourceOperationBatchOutcome::AllResults(
                        results.into_iter().flatten().collect(),
                    ),
                ))
            }
            lashlang::AbilityOp::Finish(value) => Ok(lashlang::AbilityOutcome::Value(value)),
            other => Err(lashlang::ExecutionHostError::new(format!(
                "unexpected ability {other:?}"
            ))),
        }
    }
}

/// `await [tools.step(0)?, tools.step(1)?]` — the literal-array batch path.
fn literal_array_batch_program() -> lashlang::Program {
    use lashlang::testing::ast_builders as b;
    let leaf = |id: f64| {
        b::unwrap(b::receiver_call(
            b::resource(&["tools"]),
            "step",
            vec![b::num(id)],
        ))
    };
    b::program(vec![b::finish(b::await_expr(b::list(vec![
        leaf(0.0),
        leaf(1.0),
    ])))])
}

/// The literal-array compile-time batch is a Lashlang-native aggregate: it
/// asks the host for every result and reports the **first written** rejection
/// (ADR 0099 §10 L7). Before FIG-3397 it selected the first settled one; the
/// ruling puts the dialect's own aggregates on one input-order rule, and only
/// the TypeScript `Promise.*` aggregates carry an ECMA consumer mode.
#[tokio::test]
async fn a_literal_array_batch_reports_the_first_written_rejection() {
    let host = AllResultsHost::new();
    let compiled = lashlang::testing::harness::try_compile_program(&literal_array_batch_program())
        .expect("compile the literal batch");
    let error = lashlang::execute(&compiled, &mut lashlang::State::new(), &host)
        .await
        .expect_err("both leaves reject, so the aggregate rejects");

    assert_eq!(
        host.batches.lock_recover().as_slice(),
        [2],
        "the literal array forms one batch of two leaves"
    );
    assert_eq!(
        host.consumers.lock_recover().as_slice(),
        [lashlang::AggregateConsumer::AllSettled],
        "a Lashlang-native aggregate asks for every result"
    );
    let rendered = error.to_string();
    assert!(
        rendered.contains("leaf-0 rejected"),
        "the written-first rejection is the reported one: {rendered}"
    );
    assert!(
        !rendered.contains("leaf-1 rejected"),
        "the leaf the host ran first is not the reported one: {rendered}"
    );
}

/// `await [tools.step(0)?, tools.step(1)?]` — the standalone list-batch path.
fn list_batch_program() -> lashlang::Program {
    use lashlang::testing::ast_builders as b;
    b::program(vec![b::finish(b::await_expr(b::list(vec![
        b::unwrap(b::receiver_call(
            b::resource(&["tools"]),
            "step",
            vec![b::num(0.0)],
        )),
        b::unwrap(b::receiver_call(
            b::resource(&["tools"]),
            "step",
            vec![b::num(1.0)],
        )),
    ])))])
}

/// The standalone list-batch path keeps its all-results wait and reports the
/// **first written** rejection — ruled explicitly by FIG-3397 (ADR 0099 §10
/// L7): the VM asks the host for every result and selects in written order.
#[tokio::test]
async fn the_standalone_list_batch_still_selects_the_first_written_rejection() {
    let host = AllResultsHost::new();
    let compiled = lashlang::testing::harness::try_compile_program(&list_batch_program())
        .expect("compile the list batch");
    let error = lashlang::execute(&compiled, &mut lashlang::State::new(), &host)
        .await
        .expect_err("both leaves reject, so the aggregate rejects");

    assert_eq!(
        host.batches.lock_recover().as_slice(),
        [2],
        "the list forms one batch of two leaves"
    );
    let rendered = error.to_string();
    assert!(
        rendered.contains("leaf-0 rejected"),
        "the written-first rejection is the reported one: {rendered}"
    );
    assert!(
        !rendered.contains("leaf-1 rejected"),
        "the leaf the host ran first is not consulted on this path: {rendered}"
    );
    assert_eq!(
        host.consumers.lock_recover().as_slice(),
        [lashlang::AggregateConsumer::AllSettled],
        "the list batch asks for every result"
    );
}

/// `await [[tools.step(0)?, tools.step(1)?]]` — a list whose element is
/// itself a list.
fn nested_list_program() -> lashlang::Program {
    use lashlang::testing::ast_builders as b;
    let inner = b::list(vec![
        b::unwrap(b::receiver_call(
            b::resource(&["tools"]),
            "step",
            vec![b::num(0.0)],
        )),
        b::unwrap(b::receiver_call(
            b::resource(&["tools"]),
            "step",
            vec![b::num(1.0)],
        )),
    ]);
    b::program(vec![b::finish(b::await_expr(b::list(vec![inner])))])
}

/// The nested-list law: the whole nest is one batch, and it keeps the
/// Lashlang-native written rejection order (ADR 0099 §10 L7).
///
/// One host batch of two leaves, and the written-first rejection is the one
/// reported, even though the host ran the other leaf first.
#[tokio::test]
async fn a_nested_list_is_one_batch_that_keeps_written_rejection_order() {
    let host = AllResultsHost::new();
    let compiled = lashlang::testing::harness::try_compile_program(&nested_list_program())
        .expect("compile the nested shape");
    let error = lashlang::execute(&compiled, &mut lashlang::State::new(), &host)
        .await
        .expect_err("both leaves reject, so the aggregate rejects");

    assert_eq!(
        host.batches.lock_recover().as_slice(),
        [2],
        "the nest is one batch, not one batch per inner list"
    );
    let rendered = error.to_string();
    assert!(
        rendered.contains("leaf-0 rejected"),
        "the inner template keeps written order: {rendered}"
    );
}
