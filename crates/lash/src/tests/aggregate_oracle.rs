//! Lash VM list aggregates preserve written rejection order (ADR 0099 §10 L7).

use super::*;

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
/// order leaves ran in would report leaf 1; the LashVm-native aggregates ask
/// it for every result and report in written order (ADR 0099 §10 L7).
struct AllResultsHost {
    /// The batch sizes the VM asked for, in order.
    batches: StdMutex<Vec<usize>>,
    /// The consumer mode each batch asked for.
    consumers: StdMutex<Vec<lash_vm::AggregateConsumer>>,
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

impl lash_vm::ExecutionHost for AllResultsHost {
    async fn perform(
        &self,
        op: lash_vm::AbilityOp,
    ) -> std::result::Result<lash_vm::AbilityOutcome, lash_vm::ExecutionHostError> {
        match op {
            lash_vm::AbilityOp::ResourceOperationBatch(batch) => {
                self.batches.lock_recover().push(batch.leaves.len());
                self.consumers.lock_recover().push(batch.consumer);
                self.calls.fetch_add(batch.leaves.len(), Ordering::SeqCst);
                let mut results = vec![None; batch.leaves.len()];
                for index in (0..batch.leaves.len()).rev() {
                    results[index] = Some(lash_vm::ResourceOperationOutcome::Error(
                        lash_vm::ExecutionHostError::new(format!("leaf-{index} rejected")),
                    ));
                }
                Ok(lash_vm::AbilityOutcome::ResourceOperationBatch(
                    lash_vm::ResourceOperationBatchOutcome::AllResults(
                        results.into_iter().flatten().collect(),
                    ),
                ))
            }
            lash_vm::AbilityOp::Finish(value) => Ok(lash_vm::AbilityOutcome::Value(value)),
            other => Err(lash_vm::ExecutionHostError::new(format!(
                "unexpected ability {other:?}"
            ))),
        }
    }
}

/// `await [tools.step(0)?, tools.step(1)?]` — the literal-array batch path.
fn literal_array_batch_program() -> lash_vm::Program {
    use lash_vm::testing::ast_builders as b;
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

/// The literal-array compile-time batch is a LashVm-native aggregate: it
/// asks the host for every result and reports the **first written** rejection
/// (ADR 0099 §10 L7). Before FIG-3397 it selected the first settled one; the
/// ruling puts the dialect's own aggregates on one input-order rule, and only
/// the TypeScript `Promise.*` aggregates carry an ECMA consumer mode.
#[tokio::test]
async fn a_literal_array_batch_reports_the_first_written_rejection() {
    let host = AllResultsHost::new();
    let compiled = lash_vm::testing::harness::try_compile_program(&literal_array_batch_program())
        .expect("compile the literal batch");
    let error = lash_vm::execute(&compiled, &mut lash_vm::State::new(), &host)
        .await
        .expect_err("both leaves reject, so the aggregate rejects");

    assert_eq!(
        host.batches.lock_recover().as_slice(),
        [2],
        "the literal array forms one batch of two leaves"
    );
    assert_eq!(
        host.consumers.lock_recover().as_slice(),
        [lash_vm::AggregateConsumer::AllSettled],
        "a LashVm-native aggregate asks for every result"
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
fn list_batch_program() -> lash_vm::Program {
    use lash_vm::testing::ast_builders as b;
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
    let compiled = lash_vm::testing::harness::try_compile_program(&list_batch_program())
        .expect("compile the list batch");
    let error = lash_vm::execute(&compiled, &mut lash_vm::State::new(), &host)
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
        [lash_vm::AggregateConsumer::AllSettled],
        "the list batch asks for every result"
    );
}

/// `await [[tools.step(0)?, tools.step(1)?]]` — a list whose element is
/// itself a list.
fn nested_list_program() -> lash_vm::Program {
    use lash_vm::testing::ast_builders as b;
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
/// LashVm-native written rejection order (ADR 0099 §10 L7).
///
/// One host batch of two leaves, and the written-first rejection is the one
/// reported, even though the host ran the other leaf first.
#[tokio::test]
async fn a_nested_list_is_one_batch_that_keeps_written_rejection_order() {
    let host = AllResultsHost::new();
    let compiled = lash_vm::testing::harness::try_compile_program(&nested_list_program())
        .expect("compile the nested shape");
    let error = lash_vm::execute(&compiled, &mut lash_vm::State::new(), &host)
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
