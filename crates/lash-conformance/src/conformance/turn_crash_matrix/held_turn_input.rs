//! The held pending-turn-input visibility law of the turn-crash matrix.
//!
//! Split out of `turn_crash_matrix.rs` to keep that file under the line
//! budget; the law keeps its previous path through the parent's re-export.

use super::*;
use pretty_assertions::assert_eq;

/// A crashed claim holder leaves its accepted input visible as held until the
/// matching session lease lapses, after which ordinary successor reclaim
/// redelivers it exactly once.
///
/// The crash is an aborted real runtime task at the provider mid-stream seam.
/// [`SeamStore`] suppresses the dropped guard's best-effort lease release to
/// model process loss, so the first read necessarily observes the still-live
/// abandoned lease rather than an orderly shutdown.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn held_turn_input_visibility_survives_claim_holder_crash<F, I>(
    make: F,
    make_invocation: I,
) where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
    I: Fn(&str) -> crate::ConformanceInvocation,
{
    let scenario = "held-turn-input-visibility";
    let identity = ReferenceIdentity::for_scenario(scenario);
    let raw = make(scenario);
    seed_reference_ingress(&raw, &identity, scenario).await;
    let control = SeamControl::default();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let decorated = SeamStore::wrap(raw, control.clone());
    let invocation = make_invocation(scenario);
    let effect_controller: Arc<dyn RuntimeEffectController> = Arc::new(SeamEffectController {
        inner: invocation.controller_handle(),
        control: control.clone(),
        executions: Arc::clone(&executions),
    });
    let runtime = Box::pin(build_runtime(
        decorated,
        control.clone(),
        Arc::clone(&effect_controller),
        &identity,
        TraceTool::default(),
    ))
    .await;
    let point = TurnCrashPoint {
        operation: TurnSeamOperation::Provider(ProviderOperation::InitialMidStream),
        placement: CrashPlacement::ProviderMidStream,
    };
    control.arm(point.clone());
    let task_identity = identity.clone();
    let task = crate::task::spawn(async move {
        Box::pin(drive_turn(runtime, effect_controller, &task_identity)).await
    });
    control.wait_for_hit().await;
    control.simulate_process_crash();
    task.abort();
    let abort = task
        .await
        .expect_err("the claim-holder task must be aborted");
    assert!(
        abort.is_cancelled(),
        "the task loss must be an actual abort"
    );

    let reader = make(scenario);
    super::super::bind_conformance_session(&reader, &identity.session_id).await;
    let lease_observation = reader
        .get_session_execution_lease(&identity.session_id)
        .await
        .expect("read the crashed holder's lease");
    let lease = lease_observation
        .lease
        .as_ref()
        .expect("the crash must leave its session lease held");
    assert_eq!(
        lease.owner.owner_id, CRASHED_EXECUTOR_OWNER_ID,
        "the observed lease must belong to the crashed runtime"
    );
    assert!(
        lease_observation.observed_at_epoch_ms < lease.expires_at_epoch_ms,
        "the first read must occur while the crashed holder's lease is still live"
    );
    let during_live_lease = reader
        .list_pending_turn_inputs(&identity.session_id)
        .await
        .expect("list inputs while the crashed holder's lease is live");
    assert_eq!(
        during_live_lease.len(),
        2,
        "both open rows must remain visible after the holder task aborts"
    );
    let held = during_live_lease
        .iter()
        .filter(|read| matches!(read.status, crate::PendingTurnInputReadStatus::Held { .. }))
        .collect::<Vec<_>>();
    assert_eq!(held.len(), 1, "exactly the claimed next-turn row is held");
    assert_eq!(
        pending_input_text(held[0]),
        "durable next-turn input",
        "the held row must be the input claimed before provider execution"
    );
    assert_eq!(
        held[0].status,
        crate::PendingTurnInputReadStatus::Held {
            lease_expires_at_ms: lease.expires_at_epoch_ms,
        },
        "the held read must carry the exact matching lease expiry"
    );
    let pending = during_live_lease
        .iter()
        .filter(|read| matches!(read.status, crate::PendingTurnInputReadStatus::Pending))
        .collect::<Vec<_>>();
    assert_eq!(pending.len(), 1, "the unclaimed active row remains pending");
    assert_eq!(
        pending_input_text(pending[0]),
        "active checkpoint input",
        "the status split must reflect actual claim ownership"
    );

    let successor_invocation = invocation.redrive();
    wait_for_recovery_lease(&make, scenario, &point, true).await;
    let after_expiry = reader
        .list_pending_turn_inputs(&identity.session_id)
        .await
        .expect("list inputs after lease expiry and generation turnover");
    assert_eq!(after_expiry.len(), 2, "both rows remain recoverable");
    assert!(
        after_expiry
            .iter()
            .all(|read| matches!(read.status, crate::PendingTurnInputReadStatus::Pending)),
        "an expired, released, or mismatched generation is pending, never held"
    );

    let successor_control = SeamControl::default();
    let successor_store = SeamStore::wrap(make(scenario), successor_control.clone());
    let successor_effect_controller: Arc<dyn RuntimeEffectController> =
        Arc::new(SeamEffectController {
            inner: successor_invocation.controller_handle(),
            control: successor_control.clone(),
            executions,
        });
    let successor = Box::pin(build_runtime_with_lease_timings(
        successor_store,
        successor_control.clone(),
        Arc::clone(&successor_effect_controller),
        &identity,
        TraceTool::default(),
        nominal_recovery_timings(),
    ))
    .await;
    successor_control.clear();
    let recovered = Box::pin(drive_turn(
        successor,
        successor_effect_controller,
        &identity,
    ))
    .await
    .expect("the successor redelivers the crashed holder's input")
    .expect("the recovered ingress produces a turn");
    successor_invocation.end();
    let read_model = recovered
        .state
        .read_view()
        .expect("read view after successor redrive");
    for expected in ["durable next-turn input", "active checkpoint input"] {
        let count = read_model
            .messages()
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter(|part| part.content == expected)
            .count();
        assert_eq!(
            count, 1,
            "the existing recovery mechanism must redeliver `{expected}` exactly once"
        );
    }
    assert!(
        reader
            .list_pending_turn_inputs(&identity.session_id)
            .await
            .expect("list inputs after successor settlement")
            .is_empty(),
        "successor settlement must leave no open pending input"
    );
}
