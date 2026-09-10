use super::*;
use crate::store::SessionCommitStore;

fn commit_for(session_id: &SessionId) -> crate::RuntimeCommit {
    let mut state = crate::RuntimeSessionState {
        session_id: SessionId::from(session_id.to_string()),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.ensure_agent_frame_initialized();
    crate::RuntimeCommit::persisted_state_for_test(&state, &[])
}

fn metadata_for(session_id: &SessionId) -> crate::SessionMeta {
    crate::SessionMeta {
        session_id: SessionId::from(session_id.to_string()),
        relation: crate::SessionRelation::Root,
        pending_observer_intents: Vec::new(),
    }
}

#[tokio::test]
async fn every_entry_point_reports_the_authoritative_binding() {
    let store = InMemorySessionStore::new();
    store
        .commit_runtime_state(commit_for(&SessionId::from("bound")))
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
            .save_session_meta(metadata_for(&SessionId::from("other")))
            .await
            .expect_err("metadata mismatch"),
        store
            .commit_runtime_state(commit_for(&SessionId::from("other")))
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

#[tokio::test]
async fn unbound_handle_refuses_fresh_bind_against_foreign_head_row() {
    let store = InMemorySessionStore::new();
    // Install durable head identity without binding the handle, the way a
    // store hydrated with another session's data presents itself.
    *store.session_head_meta.lock_recover() = Some(crate::SessionHeadMeta::assemble(
        crate::SessionHeadPayload {
            session_id: SessionId::from("head-session"),
            ..crate::SessionHeadPayload::default()
        },
        0,
        None,
        None,
    ));
    let error = store
        .commit_runtime_state(commit_for(&SessionId::from("other")))
        .await
        .expect_err("foreign head row must refuse a fresh bind");
    assert!(
        matches!(error, crate::StoreError::SessionBindingMismatch { ref bound_session_id, ref attempted_session_id }
        if bound_session_id == "head-session" && attempted_session_id == "other"),
        "head-row identity guards the fresh bind: {error:?}"
    );
    assert!(
        store.bound_session_id.lock_recover().is_none(),
        "a refused bind leaves the handle unbound"
    );
    // A commit matching the head row binds normally.
    store
        .commit_runtime_state(commit_for(&SessionId::from("head-session")))
        .await
        .expect("matching session binds");
    assert_eq!(
        store.bound_session_id.lock_recover().as_deref(),
        Some("head-session")
    );
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
        .replace_session_meta(metadata_for(&SessionId::from("bound")))
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
