use super::*;
use lash_core::{RuntimeSessionState, SessionId};

async fn assert_runtime_assembly_refuses_without_writes(
    backend: lash_core::Backend,
    deleted: bool,
) {
    let session_id = SessionId::from("runtime-assembly-refusal");
    let core = peer_core(backend.clone());
    let catalog = backend.session_store_factory();
    if deleted {
        core.session(session_id.clone())
            .create(crate::SessionCreation::default())
            .await
            .expect("create the deletion fixture explicitly");
        catalog
            .delete_session(&session_id)
            .await
            .expect("delete fixture");
    }
    let before = catalog
        .list_sessions(&Default::default())
        .await
        .expect("catalog before");
    let recording = Arc::new(lash_core::testing::runtime_helpers::RecordingStore::over(
        catalog.clone(),
    ));
    let store = lash_core::store::SessionStore::new(recording.clone(), session_id.clone())
        .expect("session view");
    let policy = lash_core::testing::standard_test_policy();
    let state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(policy.clone())
    };
    let plugins = lash_core::facade_support::PluginHost::new(
        lash_core::testing::test_standard_protocol_factories(),
    )
    .build_session(session_id.clone())
    .expect("runtime plugins");
    for builder in [false, true] {
        let host = lash_core::facade_support::RuntimeHostConfig::new(
            backend.clone(),
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
        let result = if builder {
            Box::pin(
                lash_core::facade_support::LashRuntime::builder(
                    host,
                    crate::testing::runtime_lease_owner(),
                )
                .with_initial_state(state.clone())
                .with_plugin_session(plugins.clone())
                .with_store(store.clone())
                .build(),
            )
            .await
        } else {
            let services = lash_core::facade_support::PersistentRuntimeServices::new(
                plugins.clone(),
                store.clone(),
                Arc::clone(&host.durability.attachment_store),
                backend.process_env_store(),
            );
            lash_core::facade_support::LashRuntime::from_persistent_embedded_state(
                policy.clone(),
                lash_core::facade_support::EmbeddedRuntimeHost::new(host),
                services,
                state.clone(),
                crate::testing::runtime_lease_owner(),
            )
            .await
        };
        assert_eq!(
            recording.session_admission_count(),
            0,
            "assembly attempted catalog admission"
        );
        assert_eq!(
            recording.commit_write_transaction_count(),
            0,
            "assembly attempted a state write"
        );
        assert!(
            recording.attachment_intents().is_empty(),
            "assembly attempted an attachment write"
        );
        assert_eq!(
            catalog
                .list_sessions(&Default::default())
                .await
                .expect("catalog after"),
            before
        );
        let error = match result {
            Ok(_) => panic!("runtime assembly accepted an unavailable session"),
            Err(error) => EmbedError::from(error),
        };
        if deleted {
            assert!(
                matches!(&error, EmbedError::Session(lash_core::SessionError::Store {
                source: lash_core::StoreError::SessionDeleted { session_id: id }, ..
            }) if id == session_id),
                "{error}"
            );
        } else {
            assert!(
                matches!(&error, EmbedError::UnknownSession { session_id: id } if id == session_id),
                "{error}"
            );
        }
    }
}

fn peer_core(backend: lash_core::Backend) -> LashCore {
    LashCore::standard_builder(backend, crate::TurnBudget::Unbounded)
        .provider(mock_provider())
        .model(mock_model_spec())
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .build(crate::testing::runtime_lease_owner())
        .expect("fixture core")
}

#[tokio::test]
async fn sqlite_runtime_assembly_refuses_absent_without_writes() {
    Box::pin(assert_runtime_assembly_refuses_without_writes(
        memory_store_backend().await,
        false,
    ))
    .await;
}

#[tokio::test]
async fn sqlite_runtime_assembly_refuses_deleted_without_writes() {
    Box::pin(assert_runtime_assembly_refuses_without_writes(
        memory_store_backend().await,
        true,
    ))
    .await;
}

#[allow(
    clippy::disallowed_methods,
    reason = "test fixture reads the PostgreSQL service URL"
)]
async fn postgres_runtime_assembly(deleted: bool) {
    let Ok(url) = std::env::var("LASH_POSTGRES_DATABASE_URL") else {
        assert!(
            std::env::var("LASH_REQUIRE_POSTGRES").is_err(),
            "PostgreSQL is required"
        );
        eprintln!("skipping PostgreSQL runtime assembly: database URL is not set");
        return;
    };
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::PostgresStorage::connect(database.url())
        .await
        .expect("PostgreSQL storage");
    let attachments = tempfile::tempdir().expect("attachment directory");
    let stores = Arc::new(lash_postgres_store::PostgresStoreSet::new(
        &storage,
        Arc::new(lash_core::facade_support::FileAttachmentStore::new(
            attachments.path(),
        )),
    ));
    Box::pin(assert_runtime_assembly_refuses_without_writes(
        lash_conformance::recording_backend_over(stores),
        deleted,
    ))
    .await;
}

#[tokio::test]
async fn postgres_runtime_assembly_refuses_absent_without_writes() {
    Box::pin(postgres_runtime_assembly(false)).await;
}

#[tokio::test]
async fn postgres_runtime_assembly_refuses_deleted_without_writes() {
    Box::pin(postgres_runtime_assembly(true)).await;
}
