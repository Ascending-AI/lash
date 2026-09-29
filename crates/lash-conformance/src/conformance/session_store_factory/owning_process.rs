use super::*;
use pretty_assertions::assert_eq;

/// A session a process's start created records that process as its owner
/// (FIG-3607 R1), and the record is the session's for as long as it lives: it
/// survives a reopen and a rewrite of the rest of the session's metadata, and
/// a session no process created has none.
#[expect(
    clippy::expect_used,
    reason = "conformance fixture: each result is established by the setup above"
)]
pub async fn session_meta_records_the_process_that_owns_it(
    factory: Arc<dyn crate::store::ConformanceDeployment>,
) {
    let owner = crate::ProcessId::fixture("session-owner");
    let owned_id = SessionId::from("session-meta-owned");
    let mut owned = session_store_request(
        &owned_id,
        "session-meta-owned-model",
        crate::SessionRelation::Child {
            parent_session_id: SessionId::from("session-meta-owner-parent"),
            caused_by: None,
        },
    );
    owned.owning_process_id = Some(owner.clone());
    let store = factory
        .admit_view(&owned)
        .await
        .expect("create the owned session");
    let meta = store
        .load_session_meta()
        .await
        .expect("load the owned session's metadata")
        .expect("the owned session has metadata");
    assert_eq!(meta.owning_process_id, Some(owner.clone()));

    // A rewrite of the rest of the record keeps the owner it was created with.
    store
        .save_session_meta(crate::SessionMeta {
            owning_process_id: None,
            pending_observer_intents: vec![crate::SessionObserverIntent::host_requested(
                crate::ProcessId::fixture("session-meta-observer"),
            )],
            ..meta
        })
        .await
        .expect("rewrite the owned session's observers");
    let reopened = factory
        .live_view_for(&owned)
        .await
        .expect("reopen the owned session")
        .expect("the owned session exists");
    assert_eq!(
        reopened
            .load_session_meta()
            .await
            .expect("reload the owned session's metadata")
            .expect("the owned session has metadata")
            .owning_process_id,
        Some(owner),
        "the owner survives a rewrite and a reopen"
    );

    let unowned_id = SessionId::from("session-meta-unowned");
    let unowned = factory
        .admit_view(&session_store_request(
            &unowned_id,
            "session-meta-unowned-model",
            crate::SessionRelation::Root,
        ))
        .await
        .expect("create a host session");
    assert_eq!(
        unowned
            .load_session_meta()
            .await
            .expect("load the host session's metadata")
            .expect("the host session has metadata")
            .owning_process_id,
        None,
        "a session no process created has no owner"
    );
}
