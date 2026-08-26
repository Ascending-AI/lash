use std::fs::{FileTimes, OpenOptions};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use lash::persistence::{
    AttachmentReclamationPolicy, AttachmentStore, EmptyRootSetPolicy, PendingTurnInputDraft,
    SessionRelation, SessionStoreCreateRequest, SessionStoreFactory, TurnInputIngress,
    TurnInputState,
};
use lash::{TurnBudget, TurnInput, runtime::SessionPolicy};
use lash_sqlite_store::{BlobArtifactDescriptor, SqliteSessionStoreFactory, Store};

use crate::retention::{
    StoreRetentionTargets, run_store_retention_pass, scheduled_attachment_policy,
};

#[tokio::test]
async fn production_retention_pass_reclaims_each_store_residue_class() {
    let data_dir = tempfile::tempdir().expect("retention data dir");
    let session_root = data_dir.path().join("lash-sessions");
    let process_registry_path = data_dir.path().join("processes.db");
    let _process_registry =
        lash_sqlite_store::SqliteProcessRegistry::open(&process_registry_path, &session_root)
            .await
            .expect("process registry");
    let factory = Arc::new(SqliteSessionStoreFactory::new_with_process_registry(
        &session_root,
        &process_registry_path,
    ));
    let request = SessionStoreCreateRequest {
        pending_observer_intents: Vec::new(),
        session_id: "retention-session".to_string(),
        relation: SessionRelation::Root,
        policy: SessionPolicy::new(TurnBudget::Unbounded),
    };
    let session_store = factory.create_store(&request).await.expect("session store");
    let cancelled = session_store
        .enqueue_pending_turn_input(
            PendingTurnInputDraft::new(
                &request.session_id,
                TurnInputIngress::NextTurn,
                TurnInput::text("settled retention evidence"),
            )
            .with_source_key("retention-source"),
        )
        .await
        .expect("enqueue retention evidence");
    session_store
        .cancel_pending_turn_input(&request.session_id, &cancelled.input_id)
        .await
        .expect("settle retention evidence");

    let gc_store = Arc::new(
        Store::open(&factory.catalog_path())
            .await
            .expect("maintenance store"),
    );
    let orphan_blob = gc_store
        .put_unrooted_artifact_blob_for_testing(
            BlobArtifactDescriptor::checkpoint_component(),
            b"unreachable checkpoint component",
        )
        .await
        .expect("unreachable store blob");

    let attachment_store = Arc::new(lash::persistence::FileAttachmentStore::new(
        data_dir.path().join("attachments"),
    ));
    let orphan_attachment = attachment_store
        .put(
            b"unreferenced attachment".to_vec(),
            lash::attachments::AttachmentCreateMeta::new(
                lash::attachments::MediaType::parse("application/octet-stream")
                    .expect("attachment media type"),
                None,
                Some("retention orphan".to_string()),
            ),
        )
        .await
        .expect("unreferenced attachment");

    let targets = StoreRetentionTargets {
        factory: Arc::clone(&factory),
        gc_store: Arc::clone(&gc_store) as Arc<dyn lash::persistence::StoreMaintenance>,
        attachment_store: Arc::clone(&attachment_store),
    };
    let report = run_store_retention_pass(
        &targets,
        std::slice::from_ref(&request.session_id),
        AttachmentReclamationPolicy {
            grace_period_ms: 0,
            empty_root_set: EmptyRootSetPolicy::AuthorizeDeleteAll,
        },
    )
    .await;

    assert!(report.failures.is_empty(), "{report:?}");
    assert_eq!(report.vacuumed.len(), 1);
    let vacuum = &report.vacuumed[0].report;
    assert_eq!(vacuum.removed_pending_turn_input_tombstone_count, 1);
    assert_eq!(report.gc.as_ref().expect("GC report").deleted_blob_count, 1);
    let attachments = report.attachments.as_ref().expect("attachment report");
    assert_eq!(attachments.reclaimed_count, 1);

    assert!(
        gc_store
            .get_blob(&orphan_blob)
            .await
            .expect("read reclaimed blob")
            .is_none(),
        "the unreachable store blob must be gone"
    );
    assert!(matches!(
        attachment_store.get(&orphan_attachment.id).await,
        Err(lash::persistence::AttachmentStoreError::NotFound(id))
            if id == orphan_attachment.id
    ));
    let replay = session_store
        .enqueue_pending_turn_input(
            PendingTurnInputDraft::new(
                &request.session_id,
                TurnInputIngress::NextTurn,
                TurnInput::text("settled retention evidence"),
            )
            .with_source_key("retention-source"),
        )
        .await
        .expect("re-enqueue after vacuum");
    assert_ne!(replay.state, TurnInputState::Cancelled);
}

#[tokio::test]
async fn scheduled_retention_refuses_a_witnessed_empty_attachment_root_set() {
    let data_dir = tempfile::tempdir().expect("retention data dir");
    let session_root = data_dir.path().join("lash-sessions");
    std::fs::create_dir_all(&session_root).expect("session store root");
    let process_registry_path = data_dir.path().join("processes.db");
    let _process_registry =
        lash_sqlite_store::SqliteProcessRegistry::open(&process_registry_path, &session_root)
            .await
            .expect("process registry");
    let factory = Arc::new(SqliteSessionStoreFactory::new_with_process_registry(
        &session_root,
        &process_registry_path,
    ));
    let gc_store = Arc::new(
        Store::open(&factory.catalog_path())
            .await
            .expect("maintenance store"),
    );
    let attachment_store = Arc::new(lash::persistence::FileAttachmentStore::new(
        data_dir.path().join("attachments"),
    ));
    let orphan = attachment_store
        .put(
            b"empty-root refusal evidence".to_vec(),
            lash::attachments::AttachmentCreateMeta::new(
                lash::attachments::MediaType::parse("application/octet-stream")
                    .expect("attachment media type"),
                None,
                Some("empty-root refusal".to_string()),
            ),
        )
        .await
        .expect("unreferenced attachment");
    let orphan_path = attachment_store
        .root()
        .join("sha256")
        .join(&orphan.id.as_str()[..2])
        .join(orphan.id.as_str());
    let older_than_retention_window = SystemTime::now()
        .checked_sub(Duration::from_secs(8 * 24 * 60 * 60))
        .expect("old attachment timestamp");
    OpenOptions::new()
        .write(true)
        .open(&orphan_path)
        .expect("open attachment for backdating")
        .set_times(FileTimes::new().set_modified(older_than_retention_window))
        .expect("backdate attachment");

    let policy = scheduled_attachment_policy();
    assert_eq!(policy.empty_root_set, EmptyRootSetPolicy::Refuse);
    let report = run_store_retention_pass(
        &StoreRetentionTargets {
            factory,
            gc_store: gc_store as Arc<dyn lash::persistence::StoreMaintenance>,
            attachment_store: Arc::clone(&attachment_store),
        },
        &[],
        policy,
    )
    .await;

    assert!(report.attachments.is_none(), "{report:?}");
    assert!(
        report.failures.iter().any(|failure| {
            failure.contains("attachment reclamation stopped: store maintenance refused:")
                && failure.contains("the live root set is empty")
                && failure.contains("partial=")
        }),
        "the scheduled pass must report the refusal loudly: {report:?}"
    );
    assert!(
        attachment_store.get(&orphan.id).await.is_ok(),
        "refusal must preserve the attachment"
    );
}
