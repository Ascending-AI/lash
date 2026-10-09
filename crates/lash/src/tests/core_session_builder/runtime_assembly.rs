use super::*;
use lash_core::plugin::PluginSessionRequest;
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
            .create(crate::SessionCreation::root(
                crate::plugins::SessionToolAccess::ambient(),
                mock_session_spec(),
            ))
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
        ..RuntimeSessionState::ambient_fixture(policy.clone())
    };
    let plugins = lash_core::facade_support::PluginHost::new(
        lash_core::testing::test_standard_protocol_factories(),
        lash_core::ExecutionBudgets::recommended(),
        lash_core::trace::TraceRuntime::new(std::sync::Arc::new(
            lash_core::facade_support::SystemClock,
        )),
    )
    .build_session(PluginSessionRequest::creation(
        session_id.clone(),
        lash_core::plugin::SessionAuthorityContext::ambient_fixture(),
    ))
    .expect("runtime plugins");
    for builder in [false, true] {
        let host = lash_core::facade_support::RuntimeHostConfig::new(
            backend.clone(),
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
            crate::tools::ToolSourcePolicy::Tolerate,
            lash_core::ExecutionBudgets::recommended(),
            lash_core::runtime::DeltaCoalescing::recommended(),
            lash_core::facade_support::DataRetentionConfig::standard(),
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
            recording.attachment_writes().is_empty(),
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
    LashCore::standard_builder(backend)
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(crate::DataRetention::standard())
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(crate::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(crate::ExecutionBudgets::recommended())
        .delta_coalescing(crate::DeltaCoalescing::recommended())
        .build(crate::testing::runtime_lease_owner())
        .expect("fixture core")
}

#[tokio::test]
async fn sqlite_runtime_assembly_refuses_absent_without_writes() {
    Box::pin(assert_runtime_assembly_refuses_without_writes(
        sqlite_memory_store_backend().await,
        false,
    ))
    .await;
}

#[tokio::test]
async fn sqlite_runtime_assembly_refuses_deleted_without_writes() {
    Box::pin(assert_runtime_assembly_refuses_without_writes(
        sqlite_memory_store_backend().await,
        true,
    ))
    .await;
}
