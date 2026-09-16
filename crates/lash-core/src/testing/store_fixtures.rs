//! Store construction helpers shared by kernel tests and certification fixtures.
use crate::*;

pub use lash_core_store::testing::store_fixtures::{
    append_conformance_event_node, bind_conformance_session,
    claim_session_execution_lease_for_test, commit_conformance_state,
    commit_runtime_state_for_test, durable_turn_address, durable_turn_scope, session_store_request,
};

/// Authorize and settle the completion gate for a direct store-deferral fixture.
/// This performs the same store-owned promise protocol as a real Native turn.
pub async fn authorize_completion_deferral_for_test(
    store: &dyn RuntimePersistence,
    fence: &SessionExecutionLeaseAuthority,
    mut commit: RuntimeCommit,
) -> Result<RuntimeCommit, RuntimeError> {
    let store_error =
        |error: StoreError| RuntimeError::new(RuntimeErrorCode::RuntimeStore, error.to_string());
    let authority = crate::runtime::effect::executor::concrete_turn_cancellation_authority(
        &store
            .turn_cancellation_authority()
            .expect("fixture store owns cancellation"),
    );
    let address = TurnAddress::new(
        &commit.session_id,
        commit
            .interrupted_turn_input_turn_id
            .as_ref()
            .expect("fixture defers a turn"),
    );
    let scope = address.execution_scope();
    let resolver = authority.resolver();
    store
        .validate_turn_cancellation_binding(
            &commit.session_id,
            fence,
            authority.binding_id(),
            &scope,
        )
        .await
        .map_err(store_error)?;
    let control =
        crate::runtime::turn_control::ActiveTurnControl::new(resolver.as_ref(), address.clone())
            .await?;
    let observed = store
        .turn_cancel_request_intent(&address)
        .await
        .map_err(store_error)?;
    assert_eq!(
        observed,
        TurnCancelIntentSnapshot::Absent,
        "completion fixture has no cancellation intent"
    );
    assert!(commit.interrupted_turn_input_cancellation.is_none());
    let authorization = control.closure_authorization(
        authority.binding_id(),
        scope,
        fence,
        observed.clone(),
        false,
        None,
    )?;
    store
        .authorize_turn_cancel_closure(fence, &authorization)
        .await
        .map_err(store_error)?;
    commit.turn_cancel_closure_settlement = Some(
        control
            .settle_authorized(resolver.as_ref(), &authorization)
            .await?,
    );
    commit.interrupted_turn_cancel_intent = Some(observed);
    commit.session_execution_lease_fence = Some(fence.clone());
    Ok(commit)
}
