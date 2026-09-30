//! The §5 barrier on the Restate controller (FIG-3598, FIG-4344): the drain
//! awaits the group index's own `Drained` notice for its rank, one
//! subscription whatever the barrier's size, never polling the index.

use super::*;
use crate::effect_group::{
    EFFECT_GROUP_STATE_FORMAT_VERSION, EFFECT_GROUP_STATE_FORMATS, EffectGroupNotice,
    EffectGroupNotification,
};
use lash_core::RuntimeErrorCode;

/// The drain awaits exactly its own rank's barrier, once, and a lifted
/// barrier, a retirement's release or a group the index has no record of all
/// admit it.
#[tokio::test]
pub(super) async fn a_drain_awaits_its_ranks_barrier_notice_once() {
    for lifted in [
        EffectGroupNotification::Drained,
        EffectGroupNotification::Retired,
        EffectGroupNotification::Absent,
    ] {
        let context = Arc::new(RecordingContext::default());
        *context.group_notice_answer.lock_recover() = Some(Ok(lifted));
        let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
        controller
            .await_group_child_drain_admission("group", 3)
            .await
            .expect("the answered barrier admits the drain");
        assert_eq!(
            *context.group_notices.lock_recover(),
            vec![("group".to_string(), EffectGroupNotice::Drained { rank: 3 })]
        );
        assert!(
            context.group_notice_turn_cancels.lock_recover().is_empty(),
            "a barrier races no turn gate"
        );
    }
}

#[tokio::test]
pub(super) async fn a_barrier_answered_as_anything_else_is_a_shape_error() {
    let context = Arc::new(RecordingContext::default());
    *context.group_notice_answer.lock_recover() = Some(Ok(EffectGroupNotification::Rank));
    let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let error = controller
        .await_group_child_drain_admission("group", 2)
        .await
        .expect_err("a rank answer is not a barrier's");
    assert_eq!(error.code, RuntimeErrorCode::RuntimeEffectGroupShape);
}

/// An index whose state carries a stored-format stamp this build does not
/// read refuses with the typed terminal error, and the controller hands that
/// refusal back as itself.
#[tokio::test]
pub(super) async fn an_index_of_another_stored_format_is_refused_typed() {
    let refusal = crate::object_state::stored_format_error(
        "effect group",
        "group",
        Some(u64::from(EFFECT_GROUP_STATE_FORMAT_VERSION) + 1),
        &EFFECT_GROUP_STATE_FORMATS,
    );
    let context = Arc::new(RecordingContext::default());
    *context.group_notice_answer.lock_recover() = Some(Err(TerminalError::new(
        serde_json::to_string(&refusal).expect("encode the refusal"),
    )));
    let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let error = controller
        .await_group_child_drain_admission("group", 2)
        .await
        .expect_err("the foreign format is refused");
    assert_eq!(
        error.code,
        RuntimeErrorCode::EngineObjectStateFormatUnsupported
    );
    assert!(error.code.is_terminal());
}

/// FIG-3672 P9: a turn-observing rank wait races the turn's durable
/// cancellation gate in the journal, and a gate win is the typed cancelled
/// await. The execution's own token races nothing here: a Restate wait is
/// cancelled only by what its journal recorded.
#[tokio::test]
pub(super) async fn a_turn_observing_rank_wait_races_the_turn_gate_and_never_a_token() {
    let turn = ExecutionScope::turn(SessionId::from("session"), TurnId::from("turn"));
    let context = Arc::new(RecordingContext::default());
    *context.group_rank_read.lock_recover() =
        Some(crate::effect_group::EffectGroupReadRankResponse::NotSettled);
    let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let mut handle =
        lash_core::EffectGroupHandle::restored("group", 2, 0).expect("a restored cursor");
    let error = controller
        .await_next_settlement(
            &mut handle,
            lash_core::TurnCancelWait::observing(
                tokio_util::sync::CancellationToken::new(),
                turn.clone(),
            ),
        )
        .await
        .expect_err("the gate won the race");
    assert_eq!(
        error.code,
        RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled
    );
    assert_eq!(handle.consumed(), 0, "a cancelled await leaves the cursor");
    assert_eq!(
        *context.group_notices.lock_recover(),
        vec![("group".to_string(), EffectGroupNotice::Rank { rank: 1 })],
        "the rank wait is the group index's rank notice"
    );
    let raced = context.group_notice_turn_cancels.lock_recover().clone();
    assert_eq!(raced.len(), 1, "one gate raced the one rank wait");
    assert_eq!(raced[0].key.scope, turn);
    assert_eq!(
        raced[0].key.wait,
        lash_core::AwaitEventWaitIdentity::TurnCancelGate
    );

    // An unobserved wait races no gate, and a cancelled token does not end
    // it: the wake resolves as recorded (here, a retirement).
    *context.group_notice_answer.lock_recover() = Some(Ok(EffectGroupNotification::Retired));
    let cancelled = tokio_util::sync::CancellationToken::new();
    cancelled.cancel();
    let error = controller
        .await_next_settlement(
            &mut handle,
            lash_core::TurnCancelWait::unobserved(cancelled),
        )
        .await
        .expect_err("the retired wake is a shape error");
    assert_eq!(error.code, RuntimeErrorCode::RuntimeEffectGroupShape);
    assert_eq!(context.group_notice_turn_cancels.lock_recover().len(), 1);
}
