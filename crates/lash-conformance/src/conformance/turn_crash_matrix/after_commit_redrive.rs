//! The after-commit redrive law of the turn crash matrix (FIG-3590, FIG-3748).
//!
//! A worker can die after its turn's final commit landed and before the drain
//! finished: before the commit's response reached it, before it released the
//! execution lane, or with the lane release itself in flight. The substrate
//! then redrives the same drain under the same identity (ADR 0069 §6).
//!
//! The drain runs through the session drive (FIG-3600): a recorded admission
//! names the root, a recorded seal raises the drive epoch, and the root's
//! recorded claim records the head it was admitted on. The redrive must not
//! re-decide any of that from the store, which has moved on: the root's input
//! is applied and the head is past the committed turn. It replays the
//! recorded admission, seal and claim, rebuilds the root on its admitted head
//! (FIG-3682), and reads the committed turn back: it asks the model nothing,
//! runs no tool, and leaves the committed head as it found it.
//!
//! The law runs its turns on the tier's runner: the crash kills the turn's
//! execution where it stands, and the redrive is the tier's next run of the
//! same scope — a fresh driver in process, a redelivery replaying the journal
//! on Restate. A provider call or tool run the redrive issued would cross the
//! seam, and a second commit would move the head; either fails the law.

use super::*;
use pretty_assertions::assert_eq;

/// Every seam placement of the drive's `trace` at which the root's final
/// commit is already durable: inside the final commit (its response lost),
/// and at and inside every store call after it. Each point says whether the
/// crashed worker still holds the execution lane there: a final commit that
/// releases the lane with the head leaves no lane behind.
fn after_commit_points(trace: &[TurnSeamOperation]) -> Vec<(String, TurnCrashPoint, bool)> {
    let final_commit = trace
        .iter()
        .rposition(|operation| {
            matches!(
                operation,
                TurnSeamOperation::Store(
                    StoreOperation::CommitFinalHead { .. }
                        | StoreOperation::ApplyTurnCancelEffectsAndConsume
                )
            )
        })
        .unwrap_or_else(|| panic!("the drive's trace holds its final commit: {trace:?}"));
    let release = TurnSeamOperation::Store(StoreOperation::ReleaseSessionExecutionLease);
    let commit_releases_lane = !trace[final_commit + 1..].contains(&release);
    let mut points = vec![(
        "final-commit-response-lost".to_string(),
        TurnCrashPoint {
            operation: trace[final_commit].clone(),
            placement: CrashPlacement::InsideCall,
        },
        !commit_releases_lane,
    )];
    for operation in &trace[final_commit + 1..] {
        for placement in [CrashPlacement::Boundary, CrashPlacement::InsideCall] {
            let point = TurnCrashPoint {
                operation: operation.clone(),
                placement,
            };
            let lane_held = !(*operation == release && placement == CrashPlacement::InsideCall);
            points.push((
                format!("after-final-commit-{}", point_key(&point)),
                point,
                lane_held,
            ));
        }
    }
    points
}

/// The reference drain through the session drive, run once to its end on a
/// fresh scenario: the seam traffic the crash points are taken from.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn drive_trace(law: &MatrixLaw<'_>) -> Vec<TurnSeamOperation> {
    let scenario = "after-commit-redrive-trace";
    let identity = ReferenceIdentity::for_scenario(scenario);
    let raw = (law.make)(scenario);
    seed_reference_ingress_for_drive(&raw, &identity, scenario).await;
    let control = SeamControl::default();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (attempt, reports) = ReferenceTurn::new(
        law.stores,
        raw,
        law.host,
        &identity,
        control.clone(),
        &executions,
        nominal_recovery_timings(),
    )
    .through_drive()
    .before_drive(SeamControl::clear)
    .reporting();
    law.runner
        .run_turn(reference_admitted_scope(&identity), attempt)
        .await;
    reference_turn::reported(reports)
        .await
        .expect("the traced drive runs")
        .ran()
        .expect("the traced drive runs its root");
    control.trace()
}

/// Crash the reference drain, run through the session drive, after its final
/// commit at every placement in [`after_commit_points`], then redrive it on
/// the same store: the redrive replays the committed root from what the first
/// execution recorded, runs nothing again and commits nothing new.
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
    let trace = Box::pin(drive_trace(&law)).await;
    for (key, point, lane_held) in after_commit_points(&trace) {
        let scenario = format!("after-commit-redrive-{key}");
        Box::pin(run_after_commit_redrive(&law, &scenario, &point, lane_held)).await;
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn run_after_commit_redrive(
    law: &MatrixLaw<'_>,
    scenario: &str,
    point: &TurnCrashPoint,
    lane_held: bool,
) {
    let make = law.make;
    let identity = ReferenceIdentity::for_scenario(scenario);
    let admitted = reference_admitted_scope(&identity);
    let raw = make(scenario);
    seed_reference_ingress_for_drive(&raw, &identity, scenario).await;
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
            .through_drive()
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
            .committed_turn_exists(&identity.turn_id)
            .await
            .expect("read the root's commit receipt"),
        "{scenario}: the root's final commit is durable before the crash"
    );
    let applied = reader
        .list_turn_input_applications(&identity.session_id)
        .await
        .expect("read the applied inputs")
        .len();

    wait_for_recovery_lease(&make, scenario, point, lane_held).await;
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
    .through_drive()
    .before_drive(SeamControl::clear)
    .reporting();
    law.runner.run_turn(admitted, successor).await;
    let drain = reference_turn::reported(redriven)
        .await
        .unwrap_or_else(|error| panic!("{scenario}: the after-commit redrive failed: {error}"));

    let crate::facade_support::QueuedTurnDrain::Ran(turn) = drain else {
        panic!("{scenario}: the redrive must replay the committed root, got {drain:?}");
    };
    assert_eq!(
        turn.assistant_output.safe_text, "trace turn complete",
        "{scenario}: the redrive reads the committed root's answer back"
    );
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        executed,
        "{scenario}: the redrive executes no effect"
    );
    // The rebuilt root replays its journal: its tool group's bookkeeping
    // replays through the host, and a commit whose response was lost is
    // re-issued and adopted by its commit identity. Neither asks the model
    // or runs the tool, and the head below proves nothing committed twice.
    let redriven = successor_control.trace();
    assert!(
        redriven.iter().all(|operation| !matches!(
            operation,
            TurnSeamOperation::Provider(_)
                | TurnSeamOperation::Effect(EffectOperation::ToolAttempt { .. })
        )),
        "{scenario}: the redrive asks the model nothing and runs no tool: {redriven:?}"
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
    assert_eq!(
        reader
            .list_turn_input_applications(&identity.session_id)
            .await
            .expect("read the applied inputs after the redrive")
            .len(),
        applied,
        "{scenario}: the redrive applies no input again"
    );
}
