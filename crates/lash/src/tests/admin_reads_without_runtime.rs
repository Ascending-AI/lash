//! Every host admin read answers from durable state on a handle whose
//! runtime no run activated: a session that never ran, and a cold reopen
//! whose recorded plugin admission this core does not build (FIG-5139,
//! ADR 0119). A plugin query, which runs plugin code, is refused typed there.
//! The runs happen on the core's node over SQLite memory stores (FIG-5307).

use super::*;
use crate::TurnInput;
use lash_protocol_types::SessionId;

/// What the durable admin reads answered.
struct AdminReads {
    execution: Option<lash_core::plugin::HydratedExecutionGraph>,
}

/// Call every host admin read on `session`, whose runtime holds no built
/// capabilities. Each must answer: none may reach for an activated runtime.
async fn read_every_admin_surface(session: &crate::LashSession) -> Result<AdminReads> {
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
    let _ = observed.list_all_process_handles().await;
    Ok(AdminReads { execution })
}

fn rlm_core(backend: lash_execution_contracts::backend::Backend, tools: bool) -> Result<LashCore> {
    let builder = explicit_ephemeral_facets(rlm_core_builder_over(backend)).serve_test_llm_profile(
        text_provider(
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
async fn every_admin_read_answers_from_durable_state_without_an_activated_runtime() -> Result<()> {
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
async fn a_plugin_query_on_a_never_run_session_is_refused_as_not_published() -> Result<()> {
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
