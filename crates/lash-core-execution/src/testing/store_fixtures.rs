use crate::*;

pub use lash_core_store::testing::store_fixtures::{
    DriveSealTestOutcome, RuntimeStoreTestDriveExt, admit_at_checkpoint_for_test,
    admit_conformance_session, admit_root_for_test, admit_root_request_for_test,
    append_conformance_event_node, commit_conformance_state, commit_runtime_state_for_test,
    durable_admission, durable_turn_address, durable_turn_scope, root_session_request,
    seal_drive_fence_for_test, session_store_request, settling_commit_for_test,
};

/// The store-backed admitted scope for a registered process row: the
/// `ProcessId` the record itself minted, never a fabricated incarnation.
/// Tests that hand a controller to the durable process worker must pin this —
/// the worker's admission CAS refuses any other pair.
pub async fn recorded_process_admission(
    registry: &dyn ProcessRegistry,
    process_id: &ProcessId,
) -> AdmittedScope {
    let record = registry
        .get_process(process_id)
        .await
        .expect("process registry read")
        .expect("process record must be registered");
    AdmittedScope::process(record.id.clone())
}

/// Authorize and settle the completion gate for a direct store-deferral fixture.
/// This performs the same promise protocol as a real turn, through
/// `authority`: the backend's effect host, which owns the turn-control
/// promises.
pub async fn authorize_completion_deferral_for_test(
    store: &dyn RuntimeStore,
    authority: &crate::TurnCancellationAuthority,
    fence: &crate::store::DriveFence,
    mut commit: RuntimeCommit,
) -> Result<RuntimeCommit, RuntimeError> {
    let store_error =
        |error: StoreError| RuntimeError::new(RuntimeErrorCode::RuntimeStore, error.to_string());
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
        None,
        None,
    )?;
    store
        .authorize_turn_cancel_closure(fence, &authorization)
        .await
        .map_err(store_error)?;
    commit.turn_cancel_closure_settlement = Some(
        control
            .settle_authorized(resolver.as_ref(), &authorization, None)
            .await?,
    );
    commit.interrupted_turn_cancel_intent = Some(observed);
    commit.drive_fence = Some(Box::new(fence.clone()));
    Ok(commit)
}
