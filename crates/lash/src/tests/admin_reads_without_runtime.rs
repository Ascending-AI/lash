//! Every host admin read answers from durable state on a handle whose
//! runtime no run activated: a session that never ran, and a cold reopen
//! whose recorded plugin admission this core does not build (FIG-5139,
//! ADR 0119). A plugin query, which runs plugin code, is refused typed there.

use super::*;
use crate::tools::{ToolManifest, ToolState};
use lash_sansio::SessionId;

const SEED: u64 = 0x5139_ad01;

/// What the durable admin reads answered.
struct AdminReads {
    tool_state: crate::SessionToolState,
    manifests: Vec<ToolManifest>,
    execution: Option<lash_core::plugin::HydratedExecutionState>,
    observed_tool_state: Option<ToolState>,
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
    let tool_state = admin.tools().state().await?;
    let manifests = admin.tools().active_manifests().await?;
    let execution = admin.state().snapshot_execution().await?;
    let _ = admin.state().export().await;
    admin.state().persist_current().await?;
    admin.state().session_state_service().await?;
    admin.config().revision().await?;
    admin.config().commands().await?;
    admin.triggers().list_all().await?;
    admin.triggers().by_source_type("timer").await?;
    admin.processes().list().await?;
    admin.processes().list_all().await?;
    let observed = session.observe();
    observed.snapshot().await?;
    let _ = observed.read_view();
    let _ = observed.list_all_process_handles().await;
    assert_eq!(
        observed.active_tool_manifests(),
        manifests,
        "the observed catalog is the recorded one"
    );
    Ok(AdminReads {
        tool_state,
        manifests,
        execution,
        observed_tool_state: observed.tool_state(),
    })
}

fn rlm_core(backend: lash_core::Backend, tools: bool) -> Result<LashCore> {
    let builder = explicit_ephemeral_facets(rlm_core_builder_over(backend)).serve_test_llm_profile(
        queued_text_provider(vec![typescript_block(
            "const kept_global = 41;\nfinish(kept_global + 1);",
        )]),
        mock_llm_profile_spec(),
    );
    let builder = if tools {
        builder.tools(Arc::new(AppTools))
    } else {
        builder
    };
    builder.build(crate::testing::runtime_lease_owner())
}

#[test]
fn every_admin_read_answers_from_durable_state_without_an_activated_runtime() -> Result<()> {
    run_async_test_on_stack_budget("admin-reads-without-runtime", || async {
        let double = restate_double(SEED).await;
        let app_lookup = lash_core::ToolId::from("tool:app_lookup");

        // A session that never ran: its head records nothing yet.
        let core = rlm_core(double.lash_backend(), true)?;
        let never_ran = core
            .session(SessionId::from("admin-reads-never-ran"))
            .created()
            .await
            .open()
            .await?;
        let reads = read_every_admin_surface(&never_ran).await?;
        assert!(reads.tool_state.recorded().is_none());
        assert!(reads.tool_state.pending().is_empty());
        assert!(reads.manifests.is_empty());
        assert!(reads.execution.is_none());
        assert!(reads.observed_tool_state.is_none());

        // A run records tool state and execution state; a cold reopen on a
        // core without the run's tool source builds no capabilities.
        let session_id = SessionId::from("admin-reads-cold-reopen");
        let ran = core
            .session(session_id.clone())
            .created()
            .await
            .open()
            .await?;
        ran.send(TurnInput::text("bind a global")).output().await?;
        Box::pin(ran.close()).await?;
        drop(never_ran);
        drop(core);

        let reopened_core = rlm_core(double.lash_backend(), false)?;
        let reopened =
            retry_when_claim_frees(|| reopened_core.session(session_id.clone()).open()).await?;
        let reads = read_every_admin_surface(&reopened).await?;
        let recorded = reads
            .tool_state
            .recorded()
            .expect("the run recorded the session's tool state");
        assert!(recorded.contains(&app_lookup));
        assert!(reads.tool_state.pending().is_empty());
        assert!(
            reads
                .manifests
                .iter()
                .any(|manifest| manifest.id == app_lookup),
            "the recorded catalog names the run's tool"
        );
        assert_eq!(reads.observed_tool_state.as_ref(), Some(recorded));
        let execution = reads
            .execution
            .expect("the head records the run's execution state");
        assert!(!execution.root.is_empty());
        Ok(())
    })
}

#[test]
fn a_plugin_query_on_a_never_run_session_is_refused_as_not_published() -> Result<()> {
    run_async_test_on_stack_budget("query-not-published", || async {
        let double = restate_double(SEED).await;
        let core = rlm_core(double.lash_backend(), true)?;
        let session_id = SessionId::from("query-not-published");
        let session = core
            .session(session_id.clone())
            .created()
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
    })
}
