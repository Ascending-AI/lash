use super::*;
use crate::store::SessionCommitStore;

fn commit_for(session_id: &str) -> crate::RuntimeCommit {
    let mut state = crate::RuntimeSessionState {
        session_id: session_id.to_string(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    crate::RuntimeCommit::persisted_state_for_test(&state, &[])
}

fn metadata_for(session_id: &str) -> crate::SessionMeta {
    crate::SessionMeta {
        session_id: session_id.to_string(),
        relation: crate::SessionRelation::Root,
        pending_observer_intents: Vec::new(),
    }
}

#[tokio::test]
async fn every_entry_point_reports_the_authoritative_binding() {
    let store = InMemorySessionStore::new();
    store
        .commit_runtime_state(commit_for("bound"))
        .await
        .expect("seed direct store");
    // Deliberately disagree with the binding: cached rows are not authorities.
    store
        .session_head_meta
        .lock_recover()
        .as_mut()
        .expect("head")
        .session_id = "stale-head".into();
    store
        .session_meta
        .lock_recover()
        .as_mut()
        .expect("metadata")
        .session_id = "stale-metadata".into();
    let errors = [
        store
            .admit_and_bind_session(&crate::SessionBinding::root("other"))
            .await
            .expect_err("admission mismatch"),
        store
            .save_session_meta(metadata_for("other"))
            .await
            .expect_err("metadata mismatch"),
        store
            .commit_runtime_state(commit_for("other"))
            .await
            .expect_err("commit mismatch"),
    ];
    for error in errors {
        assert!(
            matches!(error, crate::StoreError::SessionBindingMismatch { ref bound_session_id, ref attempted_session_id }
            if bound_session_id == "bound" && attempted_session_id == "other"),
            "binding authority: {error:?}"
        );
    }
}

#[test]
fn admission_uses_metadata_presence_without_rebinding_from_metadata() {
    let store = InMemorySessionStore::new();
    *store.bound_session_id.lock_recover() = Some("bound".into());
    assert_eq!(
        store
            .admit_and_bind_session_in_memory(&crate::SessionBinding::root("bound"))
            .expect("admit bound store without metadata"),
        crate::SessionAdmission::Created
    );
    store
        .session_meta
        .lock_recover()
        .as_mut()
        .expect("metadata")
        .session_id = "stale-metadata".into();
    assert_eq!(
        store
            .admit_and_bind_session_in_memory(&crate::SessionBinding::root("bound"))
            .expect("binding owns admission"),
        crate::SessionAdmission::Rebound
    );
    store
        .replace_session_meta(metadata_for("bound"))
        .expect("binding owns metadata replacement");
    assert_eq!(
        store
            .session_meta
            .lock_recover()
            .as_ref()
            .expect("metadata")
            .session_id,
        "bound"
    );
}
