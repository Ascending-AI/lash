//! The runtime suites that used to live here are now integration test binaries
//! under `crates/lash-core/tests/runtime/tests/`. The fixtures they shared with
//! the crate's own unit tests moved to `crate::testing`; this module keeps the
//! historical `crate::runtime::tests::{helpers, trace_capture}` paths pointing
//! at them.

pub(crate) use crate::testing::trace_capture;

pub(crate) mod helpers {
    pub(crate) use crate::testing::runtime_helpers::*;

    use crate::runtime::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn recording_factory_root_set_keeps_committed_blob() {
        let sqlite = crate::testing::memory_store_set().await;
        let factory = RecordingSessionStoreFactory::over(sqlite.session_store_factory());
        let request = crate::SessionStoreCreateRequest {
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("recording-factory-gc"),
            relation: crate::SessionRelation::Root,
            policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        };
        let store = factory.create_store(&request).await.expect("create store");
        let backend = sqlite.attachment_store();
        let attachment_backend: Arc<dyn crate::AttachmentStore> = backend.clone();
        let manifest: Arc<dyn crate::AttachmentManifest> = store.clone();
        let session = crate::SessionAttachmentStore::new(
            attachment_backend,
            manifest,
            request.session_id.clone(),
        );
        let attachment = session
            .put(
                b"recording-factory-live-blob".to_vec(),
                crate::AttachmentCreateMeta::new(
                    crate::MediaType::parse("application/octet-stream").unwrap(),
                    None,
                    None,
                ),
            )
            .await
            .expect("put attachment");
        store
            .commit_refs(&request.session_id, std::slice::from_ref(&attachment.id))
            .await
            .expect("commit attachment ref");
        assert_eq!(
            store.list_all_refs().await.unwrap(),
            vec![attachment.id.clone()]
        );

        let report = crate::reclaim_unreferenced_attachments(
            &factory,
            &*backend,
            crate::AttachmentReclamationPolicy {
                grace_period_ms: 0,
                empty_root_set: crate::EmptyRootSetPolicy::Refuse,
            },
        )
        .await
        .expect("sweep");

        assert_eq!(report.scanned_blob_count, 1);
        assert_eq!(report.reclaimed_count, 0);
        assert!(report.deleted_while_referenced.is_empty());
        crate::AttachmentStore::get(&*backend, &attachment.id)
            .await
            .expect("committed blob survives");
    }

    #[tokio::test]
    async fn test_runtime_process_registry_defaults_and_can_be_disabled() {
        let backend = crate::testing::memory_store_backend().await;
        let runtime = TestRuntime::new(&backend, mock_provider(Vec::new()))
            .build()
            .await;
        assert!(runtime.host.process_registry().is_some());

        let runtime = TestRuntime::new(&backend, mock_provider(Vec::new()))
            .without_process_registry()
            .build()
            .await;
        assert!(runtime.host.process_registry().is_none());
    }
}
