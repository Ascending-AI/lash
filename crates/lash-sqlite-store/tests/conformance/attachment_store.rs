//! `SqliteAttachmentStore`: the shared attachment-store suite on a file and on
//! an in-memory catalog, and the GC agreeing with the manifest over it.

use super::*;

use lash_core_execution::attachments::{SessionAttachmentStore, reclaim_unreferenced_attachments};
use lash_core_execution::{
    AttachmentCreateMeta, AttachmentGcFence, AttachmentReclamationPolicy, AttachmentRef,
    AttachmentSource, AttachmentStore, AttachmentStoreError, AttachmentStorePersistence,
    EmptyRootSetPolicy, Message, MessageRole, Part, RuntimeSessionState, SessionRelation,
};
use lash_sansio::MediaType;
use lash_sqlite_store::SqliteAttachmentStore;

fn open_attachment_store(path: &Path) -> Arc<dyn AttachmentStore> {
    let path = path.to_path_buf();
    Arc::new(sync_await(async move {
        let catalog = Store::open(&path).await.expect("file catalog");
        SqliteAttachmentStore::for_store(&catalog)
            .await
            .expect("file attachment store")
    })) as Arc<dyn AttachmentStore>
}

lash_conformance::attachment_store_reopenable_tests!({
    let dirs = Arc::new(Mutex::new(Vec::new()));
    (
        Arc::clone(&dirs),
        move || {
            let path = fresh_db_path(&dirs, "durable-core.db");
            lash_conformance::ReopenableAttachmentStore {
                open: open_attachment_store(&path),
                reopen: open_attachment_store(&path),
            }
        },
        AttachmentStorePersistence::Durable,
    )
});

lash_conformance::attachment_store_tests!({
    (
        (),
        || {
            Arc::new(sync_await(async {
                let catalog = Store::memory().await.expect("in-memory catalog");
                SqliteAttachmentStore::for_store(&catalog)
                    .await
                    .expect("in-memory attachment store")
            })) as Arc<dyn AttachmentStore>
        },
        AttachmentStorePersistence::Ephemeral,
    )
});

fn octet_meta() -> AttachmentCreateMeta {
    AttachmentCreateMeta::new(
        MediaType::parse("application/octet-stream").expect("media type"),
        None,
        None,
    )
}

fn state_referencing(session_id: &SessionId, reference: &AttachmentRef) -> RuntimeSessionState {
    let request = lash_core_execution::testing::store_fixtures::session_store_request(
        session_id,
        "attachment-gc",
        SessionRelation::Root,
    );
    let mut state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(request.policy)
    };
    state.ensure_agent_frame_initialized();
    state.session_graph.append_message(Message {
        id: "held-image".to_string(),
        role: MessageRole::User,
        origin: None,
        parts: Arc::new(vec![Part::attachment_part(
            "held-image-part".into(),
            String::new(),
            Some(lash_sansio::PartAttachment {
                source: AttachmentSource::Stored {
                    attachment_ref: reference.clone(),
                },
            }),
        )]),
    });
    state
}

fn checkpoint_blob_count(catalog: &Path) -> i64 {
    rusqlite::Connection::open(catalog)
        .expect("open catalog to count checkpoint blobs")
        .query_row("SELECT COUNT(*) FROM blobs", [], |row| row.get(0))
        .expect("count checkpoint blobs")
}

async fn sweep(
    factory: &SqliteSessionStoreFactory,
    backend: &Arc<dyn AttachmentStore>,
) -> lash_core_execution::attachments::AttachmentReclamationReport {
    reclaim_unreferenced_attachments(
        factory,
        backend.as_ref(),
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await
    .expect("attachment sweep")
}

/// The bytes live in the catalog that holds the manifest, so the GC's two
/// halves — the root set and the backend listing — read one database. A blob a
/// committed manifest row holds survives every sweep; a blob no row holds is
/// collected; the checkpoint bytes beside them in `blobs` are never listed as
/// attachments; and once the session that held the blob is deleted, its bytes
/// go too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_attachment_gc_never_collects_a_blob_a_manifest_row_holds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let factory = SqliteSessionStoreFactory::new(dir.path());
    let session_id = SessionId::from("attachment-gc-holder");
    let session = factory
        .create_store(
            &lash_core_execution::testing::store_fixtures::session_store_request(
                &session_id,
                "attachment-gc",
                SessionRelation::Root,
            ),
        )
        .await
        .expect("create holding session");
    let catalog = Store::open(&factory.catalog_path())
        .await
        .expect("open the session catalog");
    let backend: Arc<dyn AttachmentStore> = Arc::new(
        SqliteAttachmentStore::for_store(&catalog)
            .await
            .expect("attachment store over the session catalog"),
    );

    let held = SessionAttachmentStore::new(Arc::clone(&backend), session.clone(), &session_id)
        .put(b"held by a committed turn".to_vec(), octet_meta())
        .await
        .expect("session put records an intent and stores the bytes");
    let mut commit = lash_core_execution::store::RuntimeCommit::persisted_state_for_test(
        &state_referencing(&session_id, &held),
        &[],
    );
    commit.committed_attachment_ids = vec![held.id.clone()];
    session
        .commit_runtime_state(commit)
        .await
        .expect("commit the turn that holds the attachment");
    let unheld = backend
        .put(b"no manifest row holds this".to_vec(), octet_meta())
        .await
        .expect("store an unreferenced blob");
    let checkpoint_blobs = checkpoint_blob_count(&factory.catalog_path());
    assert!(
        checkpoint_blobs > 0,
        "the committed turn must have written checkpoint bytes for the sweep to spare"
    );
    assert_eq!(
        backend.list().await.expect("list before sweep").len(),
        2,
        "the attachment listing holds the two attachment blobs and no checkpoint bytes"
    );

    let report = sweep(&factory, &backend).await;
    assert_eq!(
        report.fence,
        AttachmentGcFence::Fenced,
        "the SQLite catalog is a fenced root authority: {report:?}"
    );
    assert_eq!(
        report.reclaimed_count, 1,
        "only the unheld blob is reclaimed: {report:?}"
    );
    assert_eq!(
        backend
            .get(&held.id)
            .await
            .expect("a blob a committed manifest row holds survives the sweep")
            .bytes,
        b"held by a committed turn".to_vec()
    );
    assert!(
        matches!(
            backend.get(&unheld.id).await,
            Err(AttachmentStoreError::NotFound(_))
        ),
        "a blob no manifest row holds is collected"
    );
    assert_eq!(
        checkpoint_blob_count(&factory.catalog_path()),
        checkpoint_blobs,
        "the attachment sweep never touches checkpoint bytes"
    );

    // A second sweep changes nothing while the row still holds the blob.
    assert_eq!(sweep(&factory, &backend).await.reclaimed_count, 0);
    backend
        .get(&held.id)
        .await
        .expect("the held blob survives a repeated sweep");

    drop(session);
    factory
        .delete_session(&session_id)
        .await
        .expect("delete the holding session");
    assert_eq!(
        sweep(&factory, &backend).await.reclaimed_count,
        1,
        "with its holder deleted, nothing roots the blob"
    );
    assert!(
        matches!(
            backend.get(&held.id).await,
            Err(AttachmentStoreError::NotFound(_))
        ),
        "the released blob is collected"
    );
}
