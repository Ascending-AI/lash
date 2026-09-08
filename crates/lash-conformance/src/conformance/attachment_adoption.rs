//! Cross-session stored references acquire receiver roots in the boundary commit.
//! Root reconciliation is the layer-1 operation also used by terminal evidence
//! reclamation, so this witness can run at every head in the stack.
use lash_core::facade_support::{
    InMemoryAttachmentStore, SessionAttachmentStore, reclaim_unreferenced_attachments,
};
use lash_core::testing::store_fixtures::session_store_request;
use lash_core::*;
use std::sync::Arc;

fn state(id: &str) -> RuntimeSessionState {
    let req = session_store_request(id, "probe", SessionRelation::Root);
    let mut state = RuntimeSessionState {
        session_id: id.into(),
        ..RuntimeSessionState::new(req.policy)
    };
    state.ensure_agent_frame_initialized();
    state
}
fn with_image(state: &mut RuntimeSessionState, reference: &AttachmentRef) {
    state.session_graph.append_message(Message {
        id: format!("image-{}", state.session_graph.nodes.len()),
        role: MessageRole::User,
        origin: None,
        parts: Arc::new(vec![Part::attachment_part(
            "image-part".into(),
            String::new(),
            Some(lash_sansio::PartAttachment {
                source: AttachmentSource::Stored {
                    attachment_ref: reference.clone(),
                },
            }),
        )]),
    });
}
async fn create(f: &Arc<dyn SessionStoreFactory>, id: &str) -> Arc<dyn RuntimePersistence> {
    f.create_store(&session_store_request(id, "probe", SessionRelation::Root))
        .await
        .unwrap()
}
async fn put(
    store: Arc<dyn RuntimePersistence>,
    bytes: Arc<dyn AttachmentStore>,
    id: &str,
    n: u8,
) -> AttachmentRef {
    SessionAttachmentStore::new(bytes, store, id)
        .put(
            [id.as_bytes(), &[n]].concat(),
            AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None),
        )
        .await
        .unwrap()
}
async fn sweep(f: &Arc<dyn SessionStoreFactory>, bytes: &Arc<dyn AttachmentStore>) -> usize {
    reclaim_unreferenced_attachments(
        f.as_ref(),
        bytes.as_ref(),
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await
    .unwrap()
    .reclaimed_count
}
pub async fn cross_owner_attachment_adoption_conformance(f: Arc<dyn SessionStoreFactory>) {
    let namespace = uuid::Uuid::new_v4();
    let owner_id = format!("adoption-owner-{namespace}");
    let receiver_id = format!("adoption-receiver-{namespace}");
    let bytes: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
    let owner = create(&f, &owner_id).await;
    let live = create(&f, &receiver_id).await;
    let r = put(owner.clone(), bytes.clone(), &owner_id, 99).await;
    let mut st = state(&owner_id);
    with_image(&mut st, &r);
    let mut c = RuntimeCommit::persisted_state_for_test(&st, &[]);
    c.committed_attachment_ids = vec![r.id.clone()];
    owner.commit_runtime_state(c).await.unwrap();
    let reader = SessionAttachmentStore::new(bytes.clone(), live.clone(), &receiver_id);
    assert!(reader.get(&r.id).await.is_ok());
    let mut live_state = state(&receiver_id);
    with_image(&mut live_state, &r);
    let mut c = RuntimeCommit::persisted_state_for_test(&live_state, &[]);
    c.committed_attachment_ids = vec![r.id.clone()];
    f.delete_session(&owner_id).await.unwrap();
    let (commit, reclaim) = tokio::join!(
        live.commit_runtime_state(c),
        f.live_attachment_refs(u64::MAX)
    );
    commit.unwrap();
    reclaim.unwrap();
    let removed = sweep(&f, &bytes).await;
    assert_eq!(removed, 0, "receiver owns a committed attachment root");
    let loaded = live.load_session().await.unwrap().unwrap();
    assert!(
        serde_json::to_string(&loaded.graph)
            .unwrap()
            .contains(r.id.as_str())
    );
    assert!(
        reader.get(&r.id).await.is_ok(),
        "committed live graph references missing bytes; GC removed {removed}"
    );
    f.delete_session(&receiver_id).await.unwrap();
    assert_eq!(
        sweep(&f, &bytes).await,
        1,
        "last receiver deletion releases the root"
    );
    adoption_fence_and_rollback(f).await;
}

async fn adoption_fence_and_rollback(f: Arc<dyn SessionStoreFactory>) {
    let session_id = format!("fenced-adoption-{}", uuid::Uuid::new_v4());
    let bytes: Arc<dyn AttachmentStore> = Arc::new(InMemoryAttachmentStore::new());
    let store = create(&f, &session_id).await;
    let mut st = state(&session_id);
    let mut ids = Vec::new();
    for byte in [17, 18] {
        let reference = bytes
            .put(
                [session_id.as_bytes(), &[byte]].concat(),
                AttachmentCreateMeta::new(MediaType::parse("image/png").unwrap(), None, None),
            )
            .await
            .unwrap();
        with_image(&mut st, &reference);
        ids.push(reference.id);
    }
    ids.sort();
    for id in &ids {
        assert_eq!(
            f.condemn_attachment(id, u64::MAX).await.unwrap(),
            AttachmentCondemnation::Condemned
        );
    }
    assert_eq!(
        f.arm_attachment_delete(&ids[1]).await.unwrap(),
        AttachmentDeleteArming::Armed
    );
    let mut commit = RuntimeCommit::persisted_state_for_test(&st, &[]);
    commit.committed_attachment_ids = ids.clone();
    let snapshot = |loaded: Option<lash_core::store::PersistedSessionRead>| {
        loaded.map(|s| {
            (
                s.head_revision,
                s.checkpoint_ref,
                serde_json::to_value(s.graph).unwrap(),
            )
        })
    };
    let before = snapshot(store.load_session().await.unwrap());
    let error = store
        .commit_runtime_state(commit.clone())
        .await
        .expect_err("armed delete refuses adoption");
    assert!(
        error.to_string().contains("physical deletion is in flight"),
        "{error}"
    );
    assert_eq!(
        snapshot(store.load_session().await.unwrap()),
        before,
        "failed adoption publishes no graph/head/checkpoint"
    );
    let roots = f.live_attachment_refs(u64::MAX).await.unwrap();
    assert!(
        ids.iter().all(|id| !roots.contains(id)),
        "failed batch leaves no attachment root"
    );
    assert_eq!(
        f.arm_attachment_delete(&ids[0]).await.unwrap(),
        AttachmentDeleteArming::Armed,
        "failed batch rolls back an earlier revocation"
    );
    for id in &ids {
        f.release_attachment_condemnation(id).await.unwrap();
        assert_eq!(
            f.condemn_attachment(id, u64::MAX).await.unwrap(),
            AttachmentCondemnation::Condemned
        );
    }
    store.commit_runtime_state(commit.clone()).await.unwrap();
    assert!(
        store
            .commit_runtime_state(commit)
            .await
            .unwrap()
            .receipt_replayed
    );
    for id in &ids {
        assert_eq!(
            f.arm_attachment_delete(id).await.unwrap(),
            AttachmentDeleteArming::Revoked,
            "successful adoption revokes unarmed deletion"
        );
        assert!(f.has_live_attachment_ref(id, u64::MAX).await.unwrap());
        assert!(bytes.get(id).await.is_ok());
    }
    assert_eq!(sweep(&f, &bytes).await, 0);
    f.delete_session(&session_id).await.unwrap();
    assert_eq!(sweep(&f, &bytes).await, 2);
}
