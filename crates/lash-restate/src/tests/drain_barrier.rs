//! The §5 barrier on the Restate controller (FIG-3598): the index names the
//! blocking siblings and the scope their wakes live under, and the drain parks
//! on each one's durable drained wake, never polling the index.

use super::*;
use crate::effect_group::{
    EFFECT_GROUP_INDEX_PROTOCOL_VERSION, EffectGroupDrainBlockersResponse,
    EffectGroupWaitResolution, drained_wait_request,
};
use lash_core::RuntimeErrorCode;

fn resolved(value: EffectGroupWaitResolution) -> Resolution {
    Resolution::Ok(serde_json::to_value(value).expect("encode the wake"))
}

#[tokio::test]
pub(super) async fn an_admitted_drain_awaits_no_wake() {
    let context = Arc::new(RecordingContext::default());
    let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    controller
        .await_group_child_drain_admission("group", 1)
        .await
        .expect("nothing blocks the drain");
    assert!(context.group_waits.lock_recover().is_empty());
}

/// Every blocker's drained wake is awaited once, keyed under the scope the
/// index retained for the group — not one re-derived from the group key.
#[tokio::test]
pub(super) async fn a_blocked_drain_parks_on_each_blockers_drained_wake_under_the_retained_scope() {
    let retained = ExecutionScope::runtime_operation("retained-wait-scope");
    let context = Arc::new(RecordingContext::default());
    *context.drain_blockers.lock_recover() = Some(Ok(EffectGroupDrainBlockersResponse::Blocked {
        wait_scope: retained.clone(),
        positions: vec![0, 2],
    }));
    for lifted in [
        EffectGroupWaitResolution::Drained,
        EffectGroupWaitResolution::Retired,
    ] {
        context.group_waits.lock_recover().clear();
        *context.group_wait_resolution.lock_recover() = Some(resolved(lifted));
        let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
        controller
            .await_group_child_drain_admission("group", 3)
            .await
            .expect("the drained or retired wakes lift the barrier");
        let awaited: Vec<_> = context
            .group_waits
            .lock_recover()
            .iter()
            .map(|request| request.key.clone())
            .collect();
        let expected: Vec<_> = [0, 2]
            .into_iter()
            .map(|position| {
                drained_wait_request(&retained, "group", position)
                    .expect("drained wake request")
                    .key
            })
            .collect();
        assert_eq!(awaited, expected);
    }
}

#[tokio::test]
pub(super) async fn a_drained_wake_resolved_as_anything_else_is_a_shape_error() {
    let context = Arc::new(RecordingContext::default());
    *context.drain_blockers.lock_recover() = Some(Ok(EffectGroupDrainBlockersResponse::Blocked {
        wait_scope: ExecutionScope::runtime_operation("group"),
        positions: vec![0],
    }));
    *context.group_wait_resolution.lock_recover() = Some(resolved(EffectGroupWaitResolution::Rank));
    let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let error = controller
        .await_group_child_drain_admission("group", 2)
        .await
        .expect_err("a rank wake is not a drained wake");
    assert_eq!(error.code, RuntimeErrorCode::RuntimeEffectGroupShape);
}

/// An index whose state another protocol version wrote refuses with the typed
/// terminal error, and the controller hands that refusal back as itself.
#[tokio::test]
pub(super) async fn an_index_of_another_protocol_version_is_refused_typed() {
    let refusal = crate::effect_group::protocol_retired_error(
        "group",
        Some(u64::from(EFFECT_GROUP_INDEX_PROTOCOL_VERSION) - 1),
    );
    let context = Arc::new(RecordingContext::default());
    *context.drain_blockers.lock_recover() = Some(Err(TerminalError::new(
        serde_json::to_string(&refusal).expect("encode the refusal"),
    )));
    let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let error = controller
        .await_group_child_drain_admission("group", 2)
        .await
        .expect_err("the retired protocol is refused");
    assert_eq!(
        error.code,
        RuntimeErrorCode::RestateEffectGroupProtocolRetired
    );
    assert!(error.code.is_terminal());
    assert!(context.group_waits.lock_recover().is_empty());
}
