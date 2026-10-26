//! A session's config transactions through the facade, on its own node
//! (FIG-4379): a resubmission under one id is the same transaction.
//! Config-change observer laws live in lash-durable-test.

use super::*;

/// A session created on `core` and opened there.
async fn opened(core: &LashCore, id: &str) -> Result<lash::LashSession> {
    let id = lash_kernel_value::identity::SessionId::parse(id).expect("nonblank host identity");
    core.session(id.clone())
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    core.session(id).open().await
}

fn budget(turns: usize) -> lash::config::ConfigTransaction {
    lash::config::ConfigTransaction::of(lash::config::SetTurnBudget {
        turn_budget: lash::TurnBudget::bounded(turns),
    })
}

/// FIG-6032: settled answers adopt the current head without scanning unrelated queued work.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settled_commands_and_config_adopt_the_current_head() -> Result<()> {
    use lash_core::testing::runtime_helpers::LayeredBackend;
    use lash_core::testing::{Script, StoreOp};

    let reads = Script::new();
    let backend = LayeredBackend::over(sqlite_memory_store_backend().await)
        .map_session_store_factory(|inner| reads.wrap("settlement-reader", inner))
        .into_backend();
    let core = standard_core_over(backend);
    let session = opened(&core, "settlement-only").await?;
    let config = session.admin().config();
    let base = config.revision().await?;
    let lash::config::ConfigSettlement::Pending(receipt) = config
        .submit(lash::config::ConfigWrite::new("budget", base), budget(7))
        .await?
    else {
        panic!("the transaction is submitted durably");
    };
    // Await the committed evidence through the undecorated durable port,
    // then stop the engine to isolate the host's reads from activation.
    let store = core.backend().session_store_factory();
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            if store
                .queued_work_batch_completion(&receipt.session_id, &receipt.batch_id)
                .await
                .expect("command completion")
                .is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the engine settles the command");
    core.shutdown().await?;
    let before = reads.calls(StoreOp::load_session_window);
    let queue_reads = reads.calls(StoreOp::list_queued_work);
    assert!(matches!(config.settle(receipt.clone()).await?,
        lash::config::ConfigSettlement::Settled(lash::config::ConfigTransactionOutcome::Applied { revision, .. })
        if revision == base + 1));
    assert!(matches!(
        session.admin().commands().settle(receipt).await?,
        lash_core::runtime::SessionCommandSettlement::Applied { .. }
    ));
    let after_settlement = reads.calls(StoreOp::load_session_window);
    assert!(
        after_settlement > before,
        "settlement adopts the current durable session window"
    );
    assert_eq!(
        reads.calls(StoreOp::list_queued_work),
        queue_reads,
        "a recorded settlement loaded unrelated queued work"
    );
    assert_eq!(
        session.policy_snapshot().turn_budget,
        lash::TurnBudget::bounded(7),
        "settlement publishes the current durable head"
    );
    let before_explicit_read = reads.calls(StoreOp::load_session_window);
    let view = session.durable().read().await?.expect("committed head");
    assert_eq!(view.policy().turn_budget, lash::TurnBudget::bounded(7));
    assert!(
        reads.calls(StoreOp::load_session_window) > before_explicit_read,
        "an explicit durable read loads the requested session window"
    );
    Ok(())
}

/// A resubmission under one id with the same content is the same command:
/// it names the first one's receipt and settles as it did. The same id with
/// other content, or written against another revision, is refused
/// `ChangedContent` and changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resubmitted_transaction_reuses_its_receipt_and_refuses_changed_content() -> Result<()> {
    let core = standard_core_over(sqlite_memory_store_backend().await);
    let session = opened(&core, "config-resubmitted").await?;
    let config = session.admin().config();
    let base = config.revision().await?;
    let write = || lash::config::ConfigWrite::new("resubmitted", base);

    let lash::config::ConfigSettlement::Pending(first) = config.submit(write(), budget(7)).await?
    else {
        panic!("a store-backed session queues its transaction");
    };
    let lash::config::ConfigSettlement::Pending(again) = config.submit(write(), budget(7)).await?
    else {
        panic!("the resubmission names a queued command");
    };
    assert_eq!(again, first, "the resubmission names the same command");
    for (what, changed) in [
        ("other content", config.submit(write(), budget(9)).await),
        (
            "another expected revision",
            config
                .submit(
                    lash::config::ConfigWrite::new("resubmitted", base + 1),
                    budget(7),
                )
                .await,
        ),
    ] {
        assert!(
            matches!(
                &changed,
                Err(lash::EmbedError::ConfigSubmit(
                    lash_core::ConfigSubmitError::ChangedContent { id }
                )) if id == "resubmitted"
            ),
            "{what}: {changed:?}"
        );
    }
    let settled = config.settle(first).await?;
    assert_eq!(
        settled,
        lash::config::ConfigSettlement::Settled(lash::config::ConfigTransactionOutcome::Applied {
            base_revision: base,
            revision: base + 1,
            outputs: vec![serde_json::Value::Null],
        }),
        "the one command applies once"
    );
    assert_eq!(config.revision().await?, base + 1);
    session.observe().snapshot().await?;
    assert_eq!(
        session.policy_snapshot().turn_budget,
        lash::TurnBudget::bounded(7)
    );
    core.shutdown().await?;
    Ok(())
}

#[cfg(feature = "rlm")]
struct DataTool;

#[cfg(feature = "rlm")]
fn data_definition() -> lash_vocabulary::tool_contract::ToolDefinition {
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        "tool:read-data",
        "report.read",
        "Read the report",
        serde_json::json!({"type":"object"}),
        serde_json::json!({"type":"integer"}),
    )
    .expect("valid schemas")
    .with_execution(std::time::Duration::from_secs(30))
}

#[cfg(feature = "rlm")]
#[async_trait]
impl ToolProvider for DataTool {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash::plugins::PluginError,
    > {
        Ok(vec![data_definition().manifest()])
    }
    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        (name == "report.read").then(|| Arc::new(data_definition().contract()))
    }
    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!(1)).into()
    }
}

/// The plugin that offers [`DataTool`] under the selection its own config
/// namespace holds.
#[cfg(feature = "rlm")]
const DATA_TOOLS: &str = "data-tools";

/// A selection that offers every tool but `report.read`.
#[cfg(feature = "rlm")]
fn without_data() -> lash::tools::ToolSelectionConfig {
    lash::tools::ToolSelectionConfig {
        tools: Some(lash::tools::ToolSelection::AllExcept(vec![
            lash_vocabulary::tool_contract::ToolId::from("tool:read-data"),
        ])),
    }
}

/// The cells of an NS-SHADOW session: bind `report`, then read it as many
/// times as the law runs a turn after the host adds the package `report`.
#[cfg(feature = "rlm")]
fn report_cells(bind: &str, read: &str, reads: usize) -> ProviderHandle {
    let mut cells = std::collections::VecDeque::from([typescript_block(&format!(
        "{bind} await control.finish(0);"
    ))]);
    cells.extend(
        std::iter::repeat_with(|| typescript_block(&format!("await control.finish({read});")))
            .take(reads),
    );
    let cells = Arc::new(StdMutex::new(cells));
    lash::testing::TestProvider::builder()
        .kind("namespace-shadow")
        .complete(move |_request| {
            let text = cells
                .lock_recover()
                .pop_front()
                .expect("the law scripts every cell");
            async move {
                Ok(LlmResponse {
                    parts: vec![text],
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle()
}

#[cfg(feature = "rlm")]
fn assert_reads_the_binding(outcome: &lash::TurnOutcome) {
    assert!(
        matches!(outcome,
        lash::TurnOutcome::Finished(lash::TurnFinish::Finished { value, .. }) if *value == serde_json::json!(42)),
        "the session's `report` binding shadows the package: {outcome:?}"
    );
}

#[cfg(feature = "rlm")]
async fn namespace_shadow_law(per_run: bool) -> Result<()> {
    for (index, (bind, read)) in [
        ("const report = 41;", "report + 1"),
        ("function report() { return 41; }", "report() + 1"),
    ]
    .into_iter()
    .enumerate()
    {
        let core =
            explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
                .serve_test_llm_profile(report_cells(bind, read, 2), mock_llm_profile_spec())
                .plugin(Arc::new(data_tools()))
                .build(lash_facade_tests::runtime_lease_owner())?;
        let id =
            SessionId::parse(format!("namespace-shadow-{per_run}-{index}")).expect("session id");
        let session = core
            .session(id)
            .create(lash::SessionCreation::root(
                mock_session_spec()
                    .plugin(DATA_TOOLS, without_data())
                    .expect("the selection encodes"),
            ))
            .await?;
        let session = core.session(session.session_id().clone()).open().await?;
        session
            .send(lash::TurnInput::text("bind report"))
            .output()
            .await?;
        if per_run {
            let outcome = session
                .send(lash::TurnInput::text("offer report"))
                .options(
                    data_tools().run_options(),
                    lash::tools::ToolSelectionConfig {
                        tools: Some(lash::tools::ToolSelection::all()),
                    },
                )
                .await?
                .outcome()
                .await?;
            let lash::SendOutcome::Settled { output, .. } = outcome else {
                panic!("a package named like a binding is accepted: {outcome:?}");
            };
            assert_reads_the_binding(&output.outcome);
        } else {
            let config = session.admin().config();
            let revision = config.revision().await?;
            let outcome = config
                .apply(
                    lash::config::ConfigWrite::new("offer-report", revision),
                    lash::config::ConfigTransaction::of(lash::tools::SetToolSelection {
                        tools: lash::tools::ToolSelection::all(),
                    }),
                )
                .await?
                .await_outcome(&config)
                .await?;
            assert!(
                matches!(
                    outcome,
                    lash::config::ConfigTransactionOutcome::Applied { .. }
                ),
                "a package named like a binding is accepted: {outcome:?}"
            );
        }
        let report = session
            .send(lash::TurnInput::text("read the binding"))
            .output()
            .await?;
        assert_reads_the_binding(&report.outcome);
        drop(session);
        core.shutdown().await?;
    }
    Ok(())
}

/// NS-SHADOW (supersedes FIG-5824's refusal): a session holding `report`
/// keeps running after its config offers a package `report`.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_configuration_offering_a_bound_root_keeps_the_binding() -> Result<()> {
    namespace_shadow_law(false).await
}

/// NS-SHADOW: a run's provider options offering a package `report` leave
/// the session's `report` binding in force.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_run_tool_selection_offering_a_bound_root_keeps_the_binding() -> Result<()> {
    namespace_shadow_law(true).await
}

#[cfg(feature = "rlm")]
fn data_tools() -> lash::tools::SelectedToolsPlugin {
    lash::tools::SelectedToolsPlugin::new(DATA_TOOLS, Arc::new(DataTool))
}

#[cfg(feature = "rlm")]
struct ChangingDataTool(Arc<std::sync::atomic::AtomicBool>);

#[cfg(feature = "rlm")]
#[async_trait]
impl ToolProvider for ChangingDataTool {
    fn tool_manifests(
        &self,
        config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash::plugins::PluginError,
    > {
        Ok({
            if self.0.load(Ordering::SeqCst) {
                DataTool.tool_manifests(config)?
            } else {
                Vec::new()
            }
        })
    }
    fn resolve_contract(
        &self,
        name: &str,
        config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        DataTool.resolve_contract(name, config)
    }
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        DataTool.execute(call).await
    }
}

/// NS-SHADOW: a session holding `report` keeps running after the host
/// installs a provider of the package `report`.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_installation_of_a_bound_root_keeps_the_binding() -> Result<()> {
    let visible = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let core =
        explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
            .serve_test_llm_profile(
                report_cells("const report = 41;", "report + 1", 1),
                mock_llm_profile_spec(),
            )
            .tools(
                lash::tool_provider_key!("tools"),
                Arc::new(ChangingDataTool(Arc::clone(&visible))),
            )
            .build(lash_facade_tests::runtime_lease_owner())?;
    let id = SessionId::parse("namespace-installation").expect("session id");
    core.session(id.clone())
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    let session = core.session(id).open().await?;
    session
        .send(lash::TurnInput::text("bind report"))
        .output()
        .await?;
    let commands = session.admin().commands();
    visible.store(true, Ordering::SeqCst);
    let receipt = commands
        .refresh_tool_catalog("offer report", "offer-report")
        .await?;
    let settlement = commands.settle(receipt).await?;
    assert!(
        !matches!(
            settlement,
            lash_core::runtime::SessionCommandSettlement::Applied {
                outcome: lash_core::runtime::SessionCommandOutcome::Failed { .. },
                ..
            }
        ),
        "a provider of a bound root installs: {settlement:?}"
    );
    let report = session
        .send(lash::TurnInput::text("read report"))
        .output()
        .await?;
    assert_reads_the_binding(&report.outcome);
    drop(session);
    core.shutdown().await?;
    Ok(())
}
