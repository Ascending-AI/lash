//! Every host admin read answers from durable state on a handle whose
//! runtime no run activated: a session that never ran, and a cold reopen
//! whose recorded plugin admission this core does not build (FIG-5139,
//! ADR 0119). A plugin query, which runs plugin code, is refused typed there.
//! The runs happen on the core's node over SQLite memory stores (FIG-5307).

use super::*;
use crate::TurnInput;
use lash_kernel_value::identity::SessionId;
use std::result::Result;

/// What the durable admin reads answered.
struct AdminReads {
    execution: Option<lash_core::plugin::HydratedExecutionGraph>,
}

/// Call every host admin read on `session`, whose runtime holds no built
/// capabilities. Each must answer: none may reach for an activated runtime.
async fn read_every_admin_surface(session: &crate::LashSession) -> crate::Result<AdminReads> {
    assert!(
        session
            .runtime
            .writer()
            .lock()
            .await
            .plugin_session()
            .is_none(),
        "precondition: no run activated this handle's runtime"
    );
    let admin = session.admin();
    let execution = admin.state().snapshot_execution().await?;
    let _ = admin.state().export().await;
    admin.state().persist_current().await?;
    admin.state().session_state_service().await?;
    admin.config().revision().await?;
    admin.config().commands().await?;
    admin.processes().list().await?;
    admin.processes().list_all().await?;
    let observed = session.observe();
    observed.snapshot().await?;
    let _ = observed.read_view();
    observed.list_all_process_handles().await?;
    Ok(AdminReads { execution })
}

fn rlm_core(
    backend: lash_execution_contracts::backend::Backend,
    tools: bool,
) -> crate::Result<LashCore> {
    let builder = explicit_ephemeral_facets(rlm_core_builder_over(backend)).serve_test_llm_profile(
        part_provider(
            "admin-reads",
            typescript_block("const kept_global = 41;\nawait control.finish(kept_global + 1);"),
        ),
        mock_llm_profile_spec(),
    );
    let builder = if tools {
        builder.tools(crate::tool_provider_key!("tools"), Arc::new(AppTools))
    } else {
        builder
    };
    builder.build(crate::tests::host::runtime_lease_owner())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_admin_read_answers_from_durable_state_without_an_activated_runtime()
-> crate::Result<()> {
    {
        let backend = sqlite_memory_store_backend().await;

        // A session that never ran: its head records nothing yet.
        let core = rlm_core(backend.clone(), true)?;
        let never_ran = created_session(&core, SessionId::from("admin-reads-never-ran"))
            .await
            .open()
            .await?;
        let reads = read_every_admin_surface(&never_ran).await?;
        assert!(reads.execution.is_none());

        // A run records execution state; a cold reopen on a core without
        // the run's tool source builds no capabilities.
        let session_id = SessionId::from("admin-reads-cold-reopen");
        let ran = created_session(&core, session_id.clone())
            .await
            .open()
            .await?;
        ran.send(TurnInput::text("bind a global")).output().await?;
        drop(ran);
        drop(never_ran);
        core.shutdown().await?;

        let reopened_core = rlm_core(backend, false)?;
        let reopened = reopened_core.session(session_id.clone()).open().await?;
        let reads = read_every_admin_surface(&reopened).await?;
        assert!(
            reads.execution.is_some(),
            "the head records the run's execution state"
        );
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plugin_query_on_a_never_run_session_is_refused_as_not_published() -> crate::Result<()> {
    {
        let core = rlm_core(sqlite_memory_store_backend().await, true)?;
        let session_id = SessionId::from("query-not-published");
        let session = created_session(&core, session_id.clone())
            .await
            .open()
            .await?;
        let refused = session
            .plugin_operations()
            .query_raw("any.query", serde_json::json!({}))
            .await
            .expect_err("no plugin view is published for a session that never ran");
        assert!(
            matches!(
                refused,
                EmbedError::Control(
                    lash_core::facade_support::PluginOperationInvokeError::NotPublished {
                        session_id: ref refused_session,
                    }
                ) if *refused_session == session_id
            ),
            "the refusal is typed and names the session: {refused:?}"
        );
        Ok(())
    }
}

/// Records the session listing boundary while all unrelated ports retain the
/// SQLite backend. FIG-7256: frame identity must not duplicate a session read.
struct ListingRegistry {
    inner: Arc<dyn lash_execution_contracts::process::ProcessRegistry>,
    calls: StdMutex<
        Vec<(
            SessionId,
            lash_execution_contracts::process::ProcessListMode,
        )>,
    >,
    fail: std::sync::atomic::AtomicBool,
}

use lash_execution_contracts::PluginError;
use lash_execution_contracts::process::{
    ProcessListFilter, ProcessListMode, ProcessObserverBy, ProcessObserverRegistry, ProcessRecord,
    ProcessSessionDeleteReport, ProcessStatusFilter,
};
use lash_vocabulary::identity::ProcessId;

type RegistryResult<T> = std::result::Result<T, PluginError>;

lash_execution_contracts::delegate_process_query!(ListingRegistry, inner);
lash_execution_contracts::delegate_process_registrar!(
    ListingRegistry,
    inner,
    registration | this,
    call | {
        let _ = this;
        call.await
    },
    event | this,
    id,
    call | {
        let _ = (this, id);
        call.await
    }
);
lash_execution_contracts::delegate_process_event_log!(ListingRegistry, inner);
lash_execution_contracts::delegate_process_lifecycle!(
    ListingRegistry,
    inner,
    event | this,
    id,
    call | {
        let _ = (this, id);
        call.await
    }
);
lash_execution_contracts::delegate_process_tool_intents!(ListingRegistry, inner);
lash_execution_contracts::delegate_process_retention!(ListingRegistry, inner);

impl lash_core_store::store::FleetFormatStore for ListingRegistry {
    fn fleet_format(&self) -> lash_core_store::store::FleetFormat {
        self.inner.fleet_format()
    }
}

impl lash_execution_contracts::process::ProcessClockRebind for ListingRegistry {}

impl ListingRegistry {
    fn record(&self, session_id: &SessionId, mode: ProcessListMode) -> RegistryResult<()> {
        self.calls.lock_recover().push((session_id.clone(), mode));
        if self.fail.load(Ordering::SeqCst) {
            return Err(PluginError::ProcessUnknown {
                process_id: ProcessId::fixture("listing-read-failure"),
            });
        }
        Ok(())
    }
}

#[async_trait]
impl ProcessObserverRegistry for ListingRegistry {
    async fn add_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        by: ProcessObserverBy,
    ) -> RegistryResult<()> {
        self.inner.add_observer(session_id, process_id, by).await
    }

    async fn remove_observer(
        &self,
        session_id: &SessionId,
        process_id: &ProcessId,
        by: ProcessObserverBy,
    ) -> RegistryResult<()> {
        self.inner.remove_observer(session_id, process_id, by).await
    }

    async fn transfer_observers(
        &self,
        from: &SessionId,
        to: &SessionId,
        ids: &[ProcessId],
        by: ProcessObserverBy,
    ) -> RegistryResult<()> {
        self.inner.transfer_observers(from, to, ids, by).await
    }

    async fn list_observed_by(
        &self,
        session_id: &SessionId,
        filter: &ProcessListFilter,
    ) -> RegistryResult<Vec<ProcessRecord>> {
        assert_eq!(filter.status, ProcessStatusFilter::Any);
        self.record(session_id, ProcessListMode::All)?;
        self.inner.list_observed_by(session_id, filter).await
    }

    async fn list_live_observed_by(
        &self,
        session_id: &SessionId,
    ) -> RegistryResult<Vec<ProcessRecord>> {
        self.record(session_id, ProcessListMode::Live)?;
        self.inner.list_live_observed_by(session_id).await
    }

    async fn observers_for_process(&self, id: &ProcessId) -> RegistryResult<Vec<SessionId>> {
        self.inner.observers_for_process(id).await
    }

    async fn delete_session_process_state(
        &self,
        session_id: &SessionId,
    ) -> RegistryResult<ProcessSessionDeleteReport> {
        self.inner.delete_session_process_state(session_id).await
    }
}

#[tokio::test]
async fn session_process_listing_reads_registry_once_even_with_a_frame() -> crate::Result<()> {
    let backend = sqlite_memory_store_backend().await;
    let registry = Arc::new(ListingRegistry {
        inner: backend.process_registry(),
        calls: StdMutex::default(),
        fail: std::sync::atomic::AtomicBool::new(false),
    });
    let core = rlm_core(backend, false)?;
    let session_id = SessionId::from("process-listing");
    let session = created_session(&core, session_id.clone())
        .await
        .open()
        .await?;
    let observed = session.observe();
    for frame in [
        Some(lash_core::FrameNodeId::new("listing-frame").unwrap()),
        None,
    ] {
        let mut observation = (*observed.runtime.observe()).clone();
        observation.process_registry = Some(registry.clone());
        observation.current_frame_node_id = frame;
        observed.runtime.observation.store(Arc::new(observation));

        for fail in [false, true] {
            registry.fail.store(fail, Ordering::SeqCst);
            registry.calls.lock_recover().clear();
            let live = observed.list_process_handles().await;
            assert_eq!(
                *registry.calls.lock_recover(),
                vec![(session_id.clone(), ProcessListMode::Live)]
            );
            registry.calls.lock_recover().clear();
            let all = observed.list_all_process_handles().await;
            assert_eq!(
                *registry.calls.lock_recover(),
                vec![(session_id.clone(), ProcessListMode::All)]
            );
            for result in [live, all] {
                if fail {
                    assert!(
                        matches!(
                            result,
                            Err(EmbedError::Plugin(PluginError::ProcessUnknown { process_id }))
                                if process_id == ProcessId::fixture("listing-read-failure")
                        ),
                        "a registry failure must retain its typed identity through the facade"
                    );
                } else {
                    assert!(result?.is_empty());
                }
            }
        }
    }

    // Deliberate absence remains distinct from a failed configured registry.
    let mut observation = (*observed.runtime.observe()).clone();
    observation.process_registry = None;
    observed.runtime.observation.store(Arc::new(observation));
    registry.calls.lock_recover().clear();
    assert!(observed.list_process_handles().await?.is_empty());
    assert!(observed.list_all_process_handles().await?.is_empty());
    assert!(registry.calls.lock_recover().is_empty());
    core.shutdown().await?;
    Ok(())
}
