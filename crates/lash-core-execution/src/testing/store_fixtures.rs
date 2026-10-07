use crate::*;

pub use lash_core_store::testing::store_fixtures::{
    admit_at_checkpoint_for_test, admit_conformance_session, admit_conformance_session_with_policy,
    append_conformance_event_node, commit_conformance_state, commit_runtime_state_for_test,
    durable_admission, durable_turn_address, durable_turn_scope, root_session_request,
    root_session_request_with_policy, session_request_from_meta_for_test, session_store_request,
    session_store_request_with_policy, settling_commit_for_test,
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
