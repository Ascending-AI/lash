//! The runtime suites that used to live here are now integration test binaries
//! under `crates/lash-core/tests/runtime/tests/`. The fixtures they shared with
//! the crate's own unit tests moved to `crate::testing`; this module keeps the
//! historical `crate::runtime::tests::helpers` path pointing at them.

pub(crate) mod helpers {
    pub(crate) use crate::testing::runtime_helpers::*;

    use crate::runtime::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn recording_factory_root_set_keeps_committed_blob() {
        let sqlite = crate::testing::sqlite_memory_store_set().await;
        let factory = RecordingDeploymentStore::over(sqlite.session_store_factory());
        let request = crate::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from("recording-factory-gc"),
            relation: crate::SessionRelation::Root,
            config: crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            )
            .into(),
            head: crate::SessionCreationHead::CommittedByCreator,
        };
        crate::store::SessionCatalogStore::admit_session(&factory, &request)
            .await
            .expect("create store");
        let store = factory
            .store_for(&request.session_id)
            .expect("the recording factory holds the admitted session");
        let backend = sqlite.attachment_store();
        let attachment_backend: Arc<dyn crate::AttachmentStore> = backend.clone();
        let manifest: Arc<dyn crate::AttachmentReferrers> = store.clone();
        let session = crate::RuntimeAttachmentStore::new(
            attachment_backend,
            manifest,
            crate::RuntimeOwner::Session(request.session_id.clone()),
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
        let session_referrer = crate::ArtifactReferrer::Session(request.session_id.clone());
        crate::AttachmentReferrers::acquire_attachment_refs(
            store.as_ref(),
            &crate::ReferrerClaim::unguarded(session_referrer.clone())
                .expect("a session claim is unguarded"),
            std::slice::from_ref(&attachment.id),
        )
        .await
        .expect("commit attachment ref");
        assert!(
            crate::AttachmentReferrers::attachment_referrers(store.as_ref(), &attachment.id)
                .await
                .unwrap()
                .contains(&session_referrer)
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
        crate::AttachmentStore::get(&*backend, &attachment.id, 32 * 1024 * 1024)
            .await
            .expect("committed blob survives");
    }

    #[tokio::test]
    async fn test_runtime_process_registry_defaults_and_can_be_disabled() {
        let backend = crate::testing::sqlite_memory_store_backend().await;
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
