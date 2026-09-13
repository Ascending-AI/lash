//! Hostile identifiers must fail before namespace lookup or mutation.
use super::*;
use lash_sansio::ProcessId;
use pretty_assertions::assert_eq;

fn malformed_attachment_ids() -> Vec<String> {
    ["../x", "/abs", "", "a\0b", "é", "e\u{301}", "．．／x"]
        .into_iter()
        .map(str::to_owned)
        .chain(["a".repeat(129)])
        .collect()
}

pub(super) async fn attachment_namespace(store: Arc<dyn AttachmentStore>) {
    let reference = store
        .put(
            vec![8, 7, 8],
            AttachmentCreateMeta::new(
                MediaType::parse("application/octet-stream").unwrap(),
                None,
                None,
            ),
        )
        .await
        .expect("seed namespace canary");
    for raw in malformed_attachment_ids() {
        // The constructor is part of every backend's contract. Even a future
        // backend must never receive a malformed namespace component.
        if let Ok(id) = AttachmentId::parse(&raw) {
            let _ = store.get(&id).await;
            panic!("hostile attachment id reached backend lookup: {raw:?}");
        }
    }
    assert_eq!(
        store
            .get(&reference.id)
            .await
            .expect("canary survives")
            .bytes,
        vec![8, 7, 8]
    );
    assert_eq!(
        store.list().await.expect("list after hostile inputs").len(),
        1
    );
}

pub(super) async fn session_namespace(factory: Arc<dyn crate::SessionStoreFactory>) {
    use super::session_store_factory::session_store_request;
    for raw in ["", "nul\0session"] {
        let request = session_store_request(
            &SessionId::from(raw),
            "hostile-model",
            crate::SessionRelation::Root,
        );
        assert!(
            matches!(
                factory.create_store(&request).await,
                Err(crate::StoreError::InvalidSessionId { .. })
            ),
            "malformed session id must be rejected before namespace mutation"
        );
        assert!(
            matches!(
                factory.read_session(&SessionId::from(raw)).await,
                Err(crate::StoreError::InvalidSessionId { .. })
            ),
            "malformed session id must be rejected before namespace lookup"
        );
        assert!(
            factory.open_existing_store(&request).await.is_err(),
            "malformed session id must not resolve through request lookup"
        );
        assert!(
            factory.delete_session(&SessionId::from(raw)).await.is_err(),
            "malformed session id must not reach deletion"
        );
        assert!(
            factory
                .session_was_deleted(&SessionId::from(raw))
                .await
                .is_err(),
            "malformed session id must not reach tombstone lookup"
        );
        assert!(
            factory
                .has_claimable_queued_work(&request, 0)
                .await
                .is_err(),
            "malformed session id must not reach queued-work lookup"
        );
        assert!(
            factory
                .open_existing_store_by_id(&SessionId::from(raw))
                .await
                .is_err(),
            "malformed session id must not resolve through id lookup"
        );
    }
    // Session names are opaque keys, not path components or SQL identifiers.
    // These valid names must remain distinct; parameter binding must prevent
    // interpreting the SQL-shaped name as syntax or collapsing path segments.
    for raw in [
        "victim",
        "../victim",
        "/victim",
        "'; DROP TABLE lash_sessions; --",
    ] {
        let request = session_store_request(
            &SessionId::from(raw),
            "hostile-model",
            crate::SessionRelation::Root,
        );
        factory
            .create_store(&request)
            .await
            .expect("admit opaque session key");
    }
    for raw in [
        "victim",
        "../victim",
        "/victim",
        "'; DROP TABLE lash_sessions; --",
    ] {
        let request = session_store_request(
            &SessionId::from(raw),
            "hostile-model",
            crate::SessionRelation::Root,
        );
        let store = factory
            .open_existing_store(&request)
            .await
            .expect("open opaque key")
            .expect("key retained");
        assert_eq!(
            store
                .load_session_meta()
                .await
                .expect("load canary")
                .expect("canary exists")
                .session_id,
            raw,
            "hostile-looking opaque keys must not alias another namespace"
        );
        for raw in malformed_attachment_ids() {
            assert!(
                AttachmentId::parse(&raw).is_err(),
                "malformed attachment id must not reach the manifest namespace"
            );
        }
    }
}

pub(super) async fn process_environment_namespace(store: Arc<dyn crate::ProcessExecutionEnvStore>) {
    let spec = crate::ProcessExecutionEnvSpec::new(
        crate::PluginOptions::default(),
        crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
    );
    let bytes = spec.to_store_bytes().expect("encode test environment");
    let owner = crate::ArtifactOwner::host("hostile-input-test");
    for raw in ["", "nul\0reference"] {
        let reference = crate::ProcessExecutionEnvRef::new(raw);
        assert!(
            store
                .publish_process_execution_env(&owner, &reference, &bytes)
                .await
                .is_err(),
            "malformed environment reference must not reach blob mutation"
        );
        assert!(
            store.get_process_execution_env(&reference).await.is_err(),
            "malformed environment reference must not reach blob lookup"
        );
    }
    for raw in [
        "canary",
        "../canary",
        "'; DROP TABLE lash_artifact_blobs; --",
    ] {
        assert!(
            store
                .publish_process_execution_env(
                    &owner,
                    &crate::ProcessExecutionEnvRef::new(raw),
                    &bytes,
                )
                .await
                .is_err(),
            "non-content-addressed environment references must be refused"
        );
    }
}

pub(super) async fn process_namespace(registry: Arc<dyn crate::ConformanceProcessRegistry>) {
    for raw in ["", " ", "nul\0process", "reserved#segment"] {
        assert!(
            registry
                .register_process(crate::ProcessRegistration::new(
                    raw,
                    crate::ProcessInput::External {
                        metadata: serde_json::Value::Null
                    },
                    crate::RecoveryContract::ExternallyOwned,
                    crate::ProcessProvenance::host(),
                    lash_core::ProcessLifecyclePolicy::new(
                        lash_core::ParentScope::Host,
                        lash_core::OnParentEnd::Abandon
                    ),
                ))
                .await
                .is_err(),
            "malformed process id must be rejected before registration"
        );
        assert!(
            registry.get_process(&ProcessId::from(raw)).await.is_err(),
            "malformed process id must be rejected before lookup"
        );
    }
    for raw in ["canary", "../canary", "'; DROP TABLE lash_processes; --"] {
        registry
            .register_process(crate::ProcessRegistration::new(
                raw,
                crate::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                crate::RecoveryContract::ExternallyOwned,
                crate::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            ))
            .await
            .expect("register opaque process key");
    }
    for raw in ["canary", "../canary", "'; DROP TABLE lash_processes; --"] {
        assert_eq!(
            registry
                .get_process(&ProcessId::from(raw))
                .await
                .expect("read opaque process key")
                .expect("canary exists")
                .id,
            raw
        );
    }
}
