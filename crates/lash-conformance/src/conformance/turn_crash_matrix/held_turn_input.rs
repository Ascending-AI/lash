//! The admitted pending-turn-input visibility law of the turn-crash matrix.
//!
//! Split out of `turn_crash_matrix.rs` to keep that file under the line
//! budget; the law keeps its previous path through the parent's re-export.

use super::*;
use pretty_assertions::assert_eq;

/// A crashed worker leaves the input its root admitted visible as
/// `Admitted{root}`, and the successor that resumes the root delivers it
/// exactly once (FIG-3927).
///
/// The crash is an aborted real runtime task at the provider mid-stream seam.
/// Nothing a worker crash does releases a row: only the root's commit or
/// terminal answers it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn admitted_turn_input_visibility_survives_worker_crash<F, I>(
    stores: Arc<dyn crate::StoreSet>,
    make: F,
    make_invocation: I,
) where
    F: Fn(&str) -> Arc<dyn RuntimeStore>,
    I: Fn(&str, crate::ExecutionScope) -> crate::ConformanceInvocation,
{
    let scenario = "held-turn-input-visibility";
    let identity = ReferenceIdentity::for_scenario(scenario);
    let raw = make(scenario);
    seed_reference_ingress(&raw, &identity, scenario).await;
    let control = SeamControl::default();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let decorated = SeamStore::wrap(raw, control.clone());
    let invocation = make_invocation(scenario, reference_turn_scope(&identity));
    let effect_controller: Arc<dyn RuntimeEffectController> = SeamLayer {
        control: control.clone(),
        executions: Arc::clone(&executions),
    }
    .over(invocation.controller_handle());
    let runtime = Box::pin(build_runtime(
        Arc::clone(&stores),
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
        .expect_err("the admitting worker task must be aborted");
    assert!(
        abort.is_cancelled(),
        "the task loss must be an actual abort"
    );

    let reader = make(scenario);
    super::super::admit_conformance_session(&reader, &identity.session_id).await;
    let during_crash = reader
        .list_pending_turn_inputs(&identity.session_id)
        .await
        .expect("list inputs after the holder's worker died");
    assert_eq!(
        during_crash.len(),
        2,
        "both open rows must remain visible after the holder task aborts"
    );
    let admitted_status = crate::PendingTurnInputReadStatus::Admitted {
        root: identity.turn_id.clone(),
    };
    let admitted = during_crash
        .iter()
        .filter(|read| read.status == admitted_status)
        .collect::<Vec<_>>();
    assert_eq!(
        admitted.len(),
        1,
        "exactly the admitted next-turn row is bound to the root"
    );
    assert_eq!(
        pending_input_text(admitted[0]),
        "durable next-turn input",
        "the admitted row must be the input the root took before provider execution"
    );
    let open = during_crash
        .iter()
        .filter(|read| matches!(read.status, crate::PendingTurnInputReadStatus::Open))
        .collect::<Vec<_>>();
    assert_eq!(open.len(), 1, "the unadmitted active row remains open");
    assert_eq!(
        pending_input_text(open[0]),
        "active checkpoint input",
        "the status split must reflect the root's actual admission"
    );

    let successor_invocation = invocation.redrive();
    let before_successor = reader
        .list_pending_turn_inputs(&identity.session_id)
        .await
        .expect("list inputs before the successor drives");
    assert_eq!(
        serde_json::to_value(&before_successor).expect("encode the reads"),
        serde_json::to_value(&during_crash).expect("encode the reads"),
        "a worker crash releases nothing: the admission holds until the root settles it"
    );

    let successor_control = SeamControl::default();
    let successor_store = SeamStore::wrap(make(scenario), successor_control.clone());
    let successor_effect_controller: Arc<dyn RuntimeEffectController> = SeamLayer {
        control: successor_control.clone(),
        executions,
    }
    .over(successor_invocation.controller_handle());
    let successor = Box::pin(build_runtime_with_lease_timings(
        stores,
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
    let read_model = recovered.state.read_view();
    for expected in ["durable next-turn input", "active checkpoint input"] {
        let count = read_model
            .messages()
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter(|part| part.content() == expected)
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
