//! The after-commit redrive law of the turn crash matrix (FIG-3590).
//!
//! A worker can die after its turn's final commit landed and before the drain
//! finished: before the commit's response reached it, before it released the
//! execution lane, or with the lane release itself in flight. The substrate
//! then redrives the same queued run under the same identity (ADR 0069 §6).
//!
//! The redrive must not re-address the committed turn from the session head:
//! the head has already advanced past it, so a turn index taken from there
//! names journal rows the original execution never wrote. On the driver path
//! the run admission pinned the turn index before any effect ran, and the
//! final commit settled that admission atomically with the head. The redrive
//! therefore resumes the settled run and returns its terminal receipt at the
//! pinned position, with no effect, provider call or further commit.
//!
//! The law runs its turns on the tier's runner: the crash kills the turn's
//! execution where it stands, and the redrive is the tier's next run of the
//! same scope — a fresh driver in process, a redelivery replaying the journal
//! on Restate. Any effect, provider call or commit the redrive issued would
//! cross the seam and fail the law.

use super::*;
use pretty_assertions::assert_eq;

/// Every seam placement at which the turn's final commit is already durable.
fn after_commit_points() -> Vec<(&'static str, TurnCrashPoint)> {
    let final_commit = TurnSeamOperation::Store(StoreOperation::CommitFinalHead {
        settles_queue: false,
        settles_turn_input: false,
        releases_lease: false,
    });
    let release = TurnSeamOperation::Store(StoreOperation::ReleaseSessionExecutionLease);
    vec![
        (
            "final-commit-response-lost",
            TurnCrashPoint {
                operation: final_commit.clone(),
                placement: CrashPlacement::InsideCall,
            },
        ),
        (
            "after-final-commit",
            TurnCrashPoint {
                operation: release.clone(),
                placement: CrashPlacement::Boundary,
            },
        ),
        (
            "lane-release-response-lost",
            TurnCrashPoint {
                operation: release,
                placement: CrashPlacement::InsideCall,
            },
        ),
    ]
}

/// Crash the reference drain after its final commit at every placement in
/// [`after_commit_points`], then redrive it on the same store: the redrive
/// returns the committed run's receipt at the turn index the admission pinned,
/// and executes and commits nothing.
pub async fn turn_crash_after_commit_redrive_replays_the_committed_receipt<F, S>(
    stores: Arc<dyn crate::StoreSet>,
    make: F,
    host: Arc<dyn crate::EffectHost>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) where
    F: Fn(&str) -> Arc<S>,
    S: RuntimePersistence + crate::store::StoreTestSupport + 'static,
{
    let make = |scenario: &str| make(scenario) as Arc<dyn RuntimePersistence>;
    let host = LawSeamHost::over(host);
    let law = MatrixLaw {
        stores: &stores,
        make: &make,
        host: &host,
        runner: &runner,
    };
    let generated = generated_points(&golden_trace());
    for (key, point) in after_commit_points() {
        assert!(
            generated.contains(&point),
            "after-commit placement {key} is not a point of the golden trace: {point:?}"
        );
        let scenario = format!("after-commit-redrive-{key}");
        Box::pin(run_after_commit_redrive(&law, &scenario, &point)).await;
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn run_after_commit_redrive(law: &MatrixLaw<'_>, scenario: &str, point: &TurnCrashPoint) {
    let make = law.make;
    let identity = ReferenceIdentity::for_scenario(scenario);
    let admitted = reference_admitted_scope(&identity);
    let raw = make(scenario);
    seed_reference_ingress(&raw, &identity, scenario).await;
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let control = SeamControl::default();
    let crash = crash_at_armed_point(&control);
    let armed = point.clone();
    law.runner
        .run_turn_until_crash(
            admitted.clone(),
            ReferenceTurn::new(
                law.stores,
                raw,
                law.host,
                &identity,
                control,
                &executions,
                crashed_turn_timings(),
            )
            .before_drive(move |control| control.arm(armed.clone()))
            .attempt(),
            crash,
        )
        .await;
    let executed = executions.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        executed, 1,
        "{scenario}: the crashed turn ran its one tool effect"
    );

    let reader = make(scenario);
    super::super::bind_conformance_session(&reader, &identity.session_id).await;
    let committed = crate::load_persisted_session_state(reader.as_ref())
        .await
        .expect("read the committed head")
        .expect("the final commit is durable before the crash");
    let committed_head = reader
        .load_session_head_meta()
        .await
        .expect("read the committed head meta")
        .expect("the committed head has meta");
    assert!(
        reader
            .pending_queued_run(&identity.session_id)
            .await
            .expect("read the pending run")
            .is_none(),
        "{scenario}: the final commit settled the run admission with the head"
    );

    wait_for_recovery_lease(&make, scenario, point, point_leaves_lane_held(point)).await;
    let successor_control = SeamControl::default();
    let (successor, redriven) = ReferenceTurn::new(
        law.stores,
        make(scenario),
        law.host,
        &identity,
        successor_control.clone(),
        &executions,
        nominal_recovery_timings(),
    )
    .before_drive(SeamControl::clear)
    .reporting();
    law.runner.run_turn(admitted, successor).await;
    let drain = reference_turn::reported(redriven)
        .await
        .unwrap_or_else(|error| panic!("{scenario}: the after-commit redrive failed: {error}"));

    let crate::facade_support::QueuedTurnDrain::Replayed(receipt) = drain else {
        panic!("{scenario}: the redrive must replay the settled run, got another drain outcome");
    };
    assert_eq!(receipt.scope.id(), identity.turn_id.as_str());
    assert_eq!(receipt.position.turn_id, identity.turn_id);
    assert_eq!(receipt.position.physical_ordinal, 0);
    assert_eq!(
        receipt.position.turn_index, committed.turn_index as u64,
        "{scenario}: the receipt addresses the committed turn at the index its admission pinned"
    );
    assert!(
        matches!(
            &receipt.terminal,
            Some(crate::store::QueuedRunTerminal::Completed { turn_id, .. })
                if *turn_id == identity.turn_id
        ),
        "{scenario}: the receipt is the committed turn's terminal: {:?}",
        receipt.terminal
    );
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        executed,
        "{scenario}: the redrive executes no effect"
    );
    let redriven = successor_control.trace();
    assert!(
        redriven.iter().all(|operation| !matches!(
            operation,
            TurnSeamOperation::Provider(_)
                | TurnSeamOperation::Effect(_)
                | TurnSeamOperation::Store(StoreOperation::CommitFinalHead { .. })
        )),
        "{scenario}: the redrive issues no provider call, effect or commit: {redriven:?}"
    );
    let head = reader
        .load_session_head_meta()
        .await
        .expect("read the head after the redrive")
        .expect("the head survives the redrive");
    assert_eq!(
        head.head_revision, committed_head.head_revision,
        "{scenario}: the redrive leaves the committed head unchanged"
    );
    let state = crate::load_persisted_session_state(reader.as_ref())
        .await
        .expect("read the state after the redrive")
        .expect("the state survives the redrive");
    assert_eq!(state.turn_index, committed.turn_index);
    assert!(
        reader
            .pending_queued_run(&identity.session_id)
            .await
            .expect("read the pending run after the redrive")
            .is_none(),
        "{scenario}: the redrive leaves no run pending"
    );
}

/// Whether the crashed worker still holds the execution lane at `point`, so
/// the recovery claim must displace it.
fn point_leaves_lane_held(point: &TurnCrashPoint) -> bool {
    !matches!(
        (&point.operation, point.placement),
        (
            TurnSeamOperation::Store(StoreOperation::ReleaseSessionExecutionLease),
            CrashPlacement::InsideCall
        )
    )
}
