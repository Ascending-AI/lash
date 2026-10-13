use super::*;

/// FIG-1559, FIG-4112: the handle reports the relation the store recorded,
/// and a create naming another parent for an existing id is refused as
/// `SessionAlreadyExists`, never absorbed into the recorded row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_relation_is_read_back_and_a_conflicting_create_is_refused() {
    let core = standard_core_over(sqlite_memory_store_backend().await);
    core.session(session("relation-child"))
        .create(crate::SessionCreation::child_of(
            SessionId::from("relation-parent"),
            mock_session_spec(),
        ))
        .await
        .expect("created");
    let child = core
        .session(session("relation-child"))
        .open()
        .await
        .expect("open");
    assert_eq!(child.parent_session_id(), Some("relation-parent"));
    drop(child);

    // A reopen names no parent and still reports the recorded relation: the
    // handle reads the durable fact.
    let reopened = core
        .session(session("relation-child"))
        .open()
        .await
        .expect("reopen");
    assert_eq!(reopened.parent_session_id(), Some("relation-parent"));
    drop(reopened);

    let error = match core
        .session(session("relation-child"))
        .create(crate::SessionCreation::child_of(
            SessionId::from("other-parent"),
            mock_session_spec(),
        ))
        .await
    {
        Ok(_) => panic!("a create naming an existing id must be refused"),
        Err(error) => error,
    };
    assert!(
        matches!(
            &error,
            crate::EmbedError::SessionAlreadyExists { session_id }
                if session_id.as_str() == "relation-child"
        ),
        "expected SessionAlreadyExists, got: {error:?}"
    );

    // The refusal left the recorded relation intact.
    let after = core
        .session(session("relation-child"))
        .open()
        .await
        .expect("open after the refusal");
    assert_eq!(after.parent_session_id(), Some("relation-parent"));
    core.shutdown().await.expect("shutdown");
}
