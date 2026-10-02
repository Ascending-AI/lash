//! A process body's worker accounting across its segment boundaries
//! (ADR 0123, FIG-4422): each boundary reserves a scope of its own and
//! starts from the totals its handover carried.

use super::WorkerRecoveryLedger;
use lash_core::store::worker_recovery::WorkerRecoveryTotals;
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;

/// A worker service over the backend's recovery store, beside the harness
/// that keeps the backend open.
async fn workers() -> (
    crate::lib_tests::DoubleProcessHarness,
    lash_vm_client::service::Service,
) {
    let harness = crate::lib_tests::double_process_harness().await;
    let workers = lash_vm_client::service::Service::default()
        .with_recovery_store(harness.backend().worker_recovery());
    (harness, workers)
}

/// Restate re-executes a segment that already handed over, from the top of
/// its handler, while its successor runs live. The re-execution's
/// reservation must neither count the successor's active worker as a lost
/// attempt nor fence the successor's settlement: sharing one scope, it did
/// both, and three such overlaps exhausted a healthy body's attempts.
#[tokio::test]
async fn a_handed_over_segment_re_execution_neither_charges_nor_fences_its_successor() {
    let (_harness, workers) = workers().await;
    let process_id = lash_sansio::ProcessId::fixture("fig-4422-overlap");
    let first = WorkerRecoveryLedger::default();

    let segment = workers
        .begin_execution_from(&first.scope(&process_id), first.totals)
        .await
        .expect("the first segment reserves");
    let handover = first.crossed(segment.budget().recovery_totals());
    segment.settle().await.expect("the first segment settles");

    let successor = workers
        .begin_execution_from(&handover.scope(&process_id), handover.totals)
        .await
        .expect("the successor reserves");
    successor
        .service()
        .mark_running()
        .await
        .expect("the successor's worker runs");

    let replay = workers
        .begin_execution_from(&first.scope(&process_id), first.totals)
        .await
        .expect("the handed-over segment's re-execution reserves");
    assert_eq!(
        replay.budget().recovery_totals(),
        WorkerRecoveryTotals {
            attempts: 1,
            ..WorkerRecoveryTotals::default()
        },
        "the re-execution counts no lost attempt"
    );
    replay.settle().await.expect("the re-execution settles");

    successor
        .service()
        .checkpoint()
        .await
        .expect("the successor's settlement is not fenced");
    successor.settle().await.expect("the successor settles");
    let resumed = workers
        .begin_execution_from(&handover.scope(&process_id), handover.totals)
        .await
        .expect("the successor resumes");
    assert_eq!(
        resumed.budget().recovery_totals(),
        WorkerRecoveryTotals {
            attempts: 1,
            ..WorkerRecoveryTotals::default()
        },
        "the successor keeps its one attempt"
    );
}

/// A boundary starts from what the body consumed before it, and a lost
/// attempt after the boundary counts on from there: consumed attempts and
/// CPU stay monotone across the whole process.
#[tokio::test]
async fn a_boundary_carries_the_totals_the_body_consumed_before_it() {
    let (_harness, workers) = workers().await;
    let process_id = lash_sansio::ProcessId::fixture("fig-4422-carry");
    let consumed = WorkerRecoveryTotals {
        attempts: 2,
        cpu_nanos: 37,
        replacement: true,
        unknown_cpu_attempts: 1,
    };
    let handover = WorkerRecoveryLedger::default().crossed(consumed);
    assert_eq!(handover.boundary, 1);
    assert!(
        !handover.totals.replacement,
        "a boundary is not a replacement"
    );
    assert_ne!(
        handover.scope(&process_id),
        WorkerRecoveryLedger::default().scope(&process_id),
        "each boundary reserves its own scope"
    );

    let lost = workers
        .begin_execution_from(&handover.scope(&process_id), handover.totals)
        .await
        .expect("the boundary reserves");
    assert_eq!(
        lost.budget().recovery_totals(),
        handover.totals,
        "the boundary starts from the carried totals"
    );
    lost.service()
        .mark_running()
        .await
        .expect("the boundary's worker runs");
    drop(lost);

    let redrive = workers
        .begin_execution_from(&handover.scope(&process_id), handover.totals)
        .await
        .expect("the redrive reserves");
    assert_eq!(
        redrive.budget().recovery_totals(),
        WorkerRecoveryTotals {
            attempts: 3,
            cpu_nanos: 37,
            replacement: false,
            unknown_cpu_attempts: 2,
        },
        "the lost attempt counts on from the carried totals"
    );
}

/// The ledger rides the handover's envelope. An envelope that carries none
/// answers fresh totals here and is refused by the handover's full decode
/// before any worker launches.
#[test]
fn the_ledger_is_read_from_the_handover_envelope() {
    let ledger = WorkerRecoveryLedger::default().crossed(WorkerRecoveryTotals {
        attempts: 1,
        cpu_nanos: 5,
        replacement: false,
        unknown_cpu_attempts: 0,
    });
    let handover = |engine_state: serde_json::Value| lash_core::SegmentHandover {
        reason: lash_core::BoundaryReason::HandOver,
        program_hash: "blake3:fixture".to_owned(),
        engine_state: serde_json::to_vec(&engine_state).expect("encode the envelope"),
    };
    assert_eq!(
        WorkerRecoveryLedger::carried(Some(&handover(serde_json::json!({
            "version": super::LASHLANG_SEGMENT_STATE_VERSION,
            "worker_recovery": ledger,
        })))),
        ledger
    );
    assert_eq!(
        WorkerRecoveryLedger::carried(Some(&handover(serde_json::json!({
            "version": super::LASHLANG_SEGMENT_STATE_VERSION,
        })))),
        WorkerRecoveryLedger::default()
    );
    assert_eq!(
        WorkerRecoveryLedger::carried(None),
        WorkerRecoveryLedger::default()
    );
}
