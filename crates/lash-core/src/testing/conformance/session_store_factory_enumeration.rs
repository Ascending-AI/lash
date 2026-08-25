use super::session_store_factory::session_store_request;
use super::*;

pub(super) async fn session_store_factory_enumeration_is_read_only_and_keeps_tombstones(
    factory: Arc<dyn crate::SessionStoreFactory>,
) {
    assert!(
        factory
            .list_sessions(&crate::SessionListFilter::default())
            .await
            .expect("enumerate an empty session catalog")
            .is_empty()
    );

    let root_request = session_store_request(
        "enumeration-root",
        "enumeration-model",
        crate::SessionRelation::Root,
    );
    let child_request = session_store_request(
        "enumeration-child",
        "enumeration-model",
        crate::SessionRelation::Child {
            parent_session_id: root_request.session_id.clone(),
            caused_by: Some(crate::CausalRef::Turn {
                session_id: root_request.session_id.clone(),
                turn_id: "enumeration-parent-turn".to_string(),
            }),
        },
    );
    let root = factory
        .create_store(&root_request)
        .await
        .expect("create enumeration root");
    factory
        .create_store(&child_request)
        .await
        .expect("create enumeration child");

    let initial = factory
        .list_sessions(&crate::SessionListFilter::default())
        .await
        .expect("enumerate admitted sessions");
    assert_eq!(initial.len(), 2);
    assert!(initial.windows(2).all(|pair| {
        (pair[0].created_at_ms, pair[0].session_id.as_str())
            <= (pair[1].created_at_ms, pair[1].session_id.as_str())
    }));
    let root_initial = initial
        .iter()
        .find(|summary| summary.session_id == root_request.session_id)
        .expect("root summary is listed");
    assert_eq!(root_initial.head_revision, 0);
    assert_eq!(root_initial.last_commit_at_ms, None);
    assert!(!root_initial.deleted);
    let child_initial = initial
        .iter()
        .find(|summary| summary.session_id == child_request.session_id)
        .expect("child summary is listed");
    assert_eq!(child_initial.relation, crate::SessionRelationKind::Child);
    assert_eq!(
        child_initial.full_relation.as_ref(),
        Some(&child_request.relation)
    );
    assert_eq!(
        child_initial.parent_session_id.as_deref(),
        Some(root_request.session_id.as_str())
    );

    let child_only = factory
        .list_sessions(&crate::SessionListFilter {
            relation: Some(crate::SessionRelationKind::Child),
            deleted: Some(false),
        })
        .await
        .expect("filter live child sessions");
    assert_eq!(
        child_only
            .iter()
            .map(|summary| summary.session_id.as_str())
            .collect::<Vec<_>>(),
        vec![child_request.session_id.as_str()]
    );

    let mut state = crate::RuntimeSessionState {
        session_id: root_request.session_id.clone(),
        ..crate::RuntimeSessionState::new(root_request.policy.clone())
    };
    state.ensure_agent_frame_initialized();
    root.commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&state, &[]))
        .await
        .expect("commit enumeration root");
    let head_before = root
        .load_session_head_meta()
        .await
        .expect("read head before enumeration")
        .expect("committed root has a head");
    let after_commit = factory
        .list_sessions(&crate::SessionListFilter::default())
        .await
        .expect("enumerate after commit");
    let root_after_commit = after_commit
        .iter()
        .find(|summary| summary.session_id == root_request.session_id)
        .expect("committed root summary is listed");
    assert_eq!(root_after_commit.head_revision, head_before.head_revision);
    assert!(
        root_after_commit
            .last_commit_at_ms
            .is_some_and(|timestamp| timestamp >= root_after_commit.created_at_ms)
    );
    let head_after = root
        .load_session_head_meta()
        .await
        .expect("read head after enumeration")
        .expect("committed root still has a head");
    assert_eq!(head_after.head_revision, head_before.head_revision);
    let first_lease = root
        .try_claim_session_execution_lease(
            &root_request.session_id,
            &crate::LeaseOwnerIdentity::opaque("enumeration-proof", "first"),
            "session-enumeration-proof-executor",
            60_000,
        )
        .await
        .expect("claim after enumeration")
        .acquired()
        .expect("enumeration did not acquire the lease");
    assert_eq!(
        first_lease.fencing_token, 1,
        "enumeration must not create or advance the execution lease generation"
    );

    factory
        .delete_session(&root_request.session_id)
        .await
        .expect("delete enumerated root");
    let tombstones = factory
        .list_sessions(&crate::SessionListFilter {
            relation: None,
            deleted: Some(true),
        })
        .await
        .expect("enumerate deletion tombstones");
    assert_eq!(tombstones.len(), 1);
    assert_eq!(tombstones[0].session_id, root_request.session_id);
    assert!(tombstones[0].deleted);
    assert_eq!(tombstones[0].full_relation, None);
    assert_eq!(tombstones[0].head_revision, head_before.head_revision);
    root.vacuum()
        .await
        .expect("vacuum stale root handle after enumeration");
    assert!(
        factory
            .list_sessions(&crate::SessionListFilter {
                relation: None,
                deleted: Some(true),
            })
            .await
            .expect("enumerate tombstones after vacuum")
            .iter()
            .any(|summary| summary.session_id == root_request.session_id),
        "vacuum must not erase permanent session enumeration evidence"
    );
}
