//! A session's config transactions through the facade, on its own node
//! (FIG-4379): a resubmission under one id is the same transaction, and
//! every applied transaction is announced to the session's plugins.

use super::*;

/// A session created on `core` and opened there.
async fn opened(core: &LashCore, id: &str) -> Result<crate::LashSession> {
    let id = crate::SessionId::parse(id).expect("nonblank host identity");
    core.session(id.clone())
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await?;
    core.session(id).open().await
}

fn budget(turns: usize) -> crate::config::ConfigTransaction {
    crate::config::ConfigTransaction::of(crate::config::SetTurnBudget {
        turn_budget: crate::TurnBudget::bounded(turns),
    })
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
    let write = || crate::config::ConfigWrite::new("resubmitted", base);

    let crate::config::ConfigSettlement::Pending(first) = config.submit(write(), budget(7)).await?
    else {
        panic!("a store-backed session queues its transaction");
    };
    let crate::config::ConfigSettlement::Pending(again) = config.submit(write(), budget(7)).await?
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
                    crate::config::ConfigWrite::new("resubmitted", base + 1),
                    budget(7),
                )
                .await,
        ),
    ] {
        assert!(
            matches!(
                &changed,
                Err(crate::EmbedError::ConfigSubmit(
                    lash_core::ConfigSubmitError::ChangedContent { id }
                )) if id == "resubmitted"
            ),
            "{what}: {changed:?}"
        );
    }
    let settled = config.settle(first).await?;
    assert_eq!(
        settled,
        crate::config::ConfigSettlement::Settled(
            crate::config::ConfigTransactionOutcome::Applied {
                base_revision: base,
                revision: base + 1,
                outputs: vec![serde_json::Value::Null],
            }
        ),
        "the one command applies once"
    );
    assert_eq!(config.revision().await?, base + 1);
    assert_eq!(
        session.policy_snapshot().turn_budget,
        crate::TurnBudget::bounded(7)
    );
    core.shutdown().await?;
    Ok(())
}

/// What a plugin's `SessionConfigChanged` observer was handed: the policy
/// before and after each applied transaction.
type Changes = Arc<StdMutex<Vec<(lash_core::SessionPolicy, lash_core::SessionPolicy)>>>;

/// Every applied core command announces `SessionConfigChanged` to the
/// session's plugins with the policy before and after it, whatever it
/// changed: the model binding (to another key, or another transport for one
/// wire model), generation and the turn budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-5317: the durable session actor applies a config transaction without building its plugins, so no SessionConfigChanged observer runs"]
async fn every_applied_config_transaction_emits_a_lifecycle_event() -> Result<()> {
    let changes: Changes = Arc::default();
    let observer = {
        let changes = Arc::clone(&changes);
        StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("config-observer"),
            lash_core::facade_support::PluginSpec::new().with_runtime_event(
                crate::hook_key!("config-changed"),
                Arc::new(move |event| {
                    let changes = Arc::clone(&changes);
                    Box::pin(async move {
                        if let lash_core::plugin::PluginLifecycleEvent::SessionConfigChanged(ctx) =
                            event
                        {
                            changes
                                .lock_recover()
                                .push((ctx.previous.clone(), ctx.current.clone()));
                        }
                        Ok(())
                    })
                }),
            ),
        )
    };
    let alt = lash_core::LlmProfileMetadata::builder("alt-model")
        .cache_retention(lash_core::provider::CacheRetention::Short)
        .context_window_tokens(123_456)
        .build()
        .expect("valid model metadata");
    let unwired = |kind: &'static str| {
        crate::testing::TestProvider::builder()
            .kind(kind)
            .complete_error("not wired")
            .build()
            .into_handle()
    };
    // Two keys share one wire model on different transports: a model change
    // moves the transport only through the key the registry minted.
    let models = lash_core::LlmProfileRegistry::new()
        .register(
            mock_llm_profile_spec().wire_model.clone(),
            lash_core::RegisteredLlmProfile::new(mock_llm_profile_spec(), mock_provider()),
        )
        .expect("register the mock model")
        .register(
            "alt-model",
            lash_core::RegisteredLlmProfile::new(alt.clone(), unwired("alt")),
        )
        .expect("register alt-model")
        .register(
            "alt-model-on-alt",
            lash_core::RegisteredLlmProfile::new(alt, unwired("alt-on-alt")),
        )
        .expect("register alt-model-on-alt");
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .llm_profiles(Arc::new(models))
    .plugin(Arc::new(observer))
    .build(crate::testing::runtime_lease_owner())?;
    let session = opened(&core, "config-lifecycle").await?;
    let config = session.admin().config();
    let generation = lash_core::GenerationOptions {
        seed: Some(42),
        ..Default::default()
    };
    let transactions = [
        crate::config::ConfigTransaction::of(crate::config::SetLlmProfile {
            model: crate::LlmProfileKey::new("alt-model"),
        }),
        crate::config::ConfigTransaction::of(crate::config::SetLlmProfile {
            model: crate::LlmProfileKey::new("alt-model-on-alt"),
        }),
        crate::config::ConfigTransaction::of(crate::config::SetGeneration {
            generation: lash_core::facade_support::GenerationOverlay::Replace(generation.clone()),
        }),
        budget(9),
    ];
    for (index, transaction) in transactions.into_iter().enumerate() {
        let revision = config.revision().await?;
        let outcome = config
            .apply(
                crate::config::ConfigWrite::new(format!("lifecycle-{index}"), revision),
                transaction,
            )
            .await?
            .await_outcome(&config)
            .await?;
        assert!(
            matches!(
                outcome,
                crate::config::ConfigTransactionOutcome::Applied { .. }
            ),
            "{index}: {outcome:?}"
        );
    }

    let changes = changes.lock_recover().clone();
    assert_eq!(
        changes.len(),
        4,
        "every applied transaction is announced once"
    );
    let key = |policy: &lash_core::SessionPolicy| {
        policy
            .profile_key()
            .map(ToString::to_string)
            .unwrap_or_default()
    };
    let (previous, current) = &changes[0];
    assert_eq!(key(previous), mock_llm_profile_spec().wire_model);
    assert_eq!(key(current), "alt-model");
    assert_ne!(
        previous.context_window_tokens(),
        current.context_window_tokens()
    );
    let (previous, current) = &changes[1];
    assert_eq!(key(previous), "alt-model");
    assert_eq!(key(current), "alt-model-on-alt");
    assert_eq!(previous.wire_model(), current.wire_model());
    let (_, current) = &changes[2];
    assert_eq!(current.generation, generation);
    let (previous, current) = &changes[3];
    assert_eq!(previous.generation, generation);
    assert_eq!(current.turn_budget, crate::TurnBudget::bounded(9));
    core.shutdown().await?;
    Ok(())
}

#[cfg(feature = "rlm")]
struct DataTool;

#[cfg(feature = "rlm")]
fn data_definition() -> lash_core::ToolDefinition {
    use lash_core::ToolDefinitionBindingExt as _;
    lash_core::ToolDefinition::raw(
        "tool:read-data",
        "read_data",
        "Read data",
        serde_json::json!({"type":"object"}),
        serde_json::json!({}),
    )
    .expect("valid schemas")
    .with_execution(std::time::Duration::from_secs(30))
    .with_tool_binding(lash_core::ToolBinding::new(["data"], "read"))
}

#[cfg(feature = "rlm")]
#[async_trait]
impl ToolProvider for DataTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![data_definition().manifest()]
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "read_data").then(|| Arc::new(data_definition().contract()))
    }
    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!(1)).into()
    }
}

#[cfg(feature = "rlm")]
async fn namespace_collision_law(per_run: bool) -> Result<()> {
    for (index, (bind, read)) in [
        ("const data = 41;", "data + 1"),
        ("function data() { return 41; }", "data() + 1"),
    ]
    .into_iter()
    .enumerate()
    {
        let cells = Arc::new(StdMutex::new(std::collections::VecDeque::from([
            typescript_block(&format!("{bind} await control.finish(0);")),
            typescript_block(&format!("await control.finish({read});")),
        ])));
        let provider = crate::testing::TestProvider::builder()
            .kind("namespace-collision")
            .complete(move |_request| {
                let text = cells
                    .lock_recover()
                    .pop_front()
                    .expect("only accepted runs call the model");
                async move { Ok(text_response(&text)) }
            })
            .build()
            .into_handle();
        let core =
            explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
                .serve_test_llm_profile(provider, mock_llm_profile_spec())
                .tools(Arc::new(DataTool))
                .build(crate::testing::runtime_lease_owner())?;
        let id =
            SessionId::parse(format!("namespace-collision-{per_run}-{index}")).expect("session id");
        let hidden = lash_core::SessionToolAccess::ambient()
            .with_hidden_tools(["read_data"])
            .expect("valid tool name");
        let session = core
            .session(id)
            .create(crate::SessionCreation::root(hidden, mock_session_spec()))
            .await?;
        let session = core.session(session.session_id().clone()).open().await?;
        session
            .send(crate::TurnInput::text("bind data"))
            .output()
            .await?;
        if per_run {
            let outcome = session
                .send(crate::TurnInput::text("offer data"))
                .tool_access(lash_core::SessionToolAccess::ambient())
                .await?
                .outcome()
                .await?;
            let crate::SendOutcome::Refused { refusal, .. } = outcome else {
                panic!("collision must refuse the run");
            };
            let Some(lash_core::RunShapeRefusal::Owner { refusal }) = refusal.run_shape_refusal()
            else {
                panic!("the run retains the typed catalog refusal: {refusal:?}");
            };
            assert_namespace_collision(refusal);
        } else {
            let config = session.admin().config();
            let revision = config.revision().await?;
            let outcome = config
                .apply(
                    crate::config::ConfigWrite::new("offer-data", revision),
                    crate::config::ConfigTransaction::of(crate::config::SetToolAccess {
                        access: lash_core::SessionToolAccess::ambient(),
                    }),
                )
                .await?
                .await_outcome(&config)
                .await?;
            let crate::config::ConfigTransactionOutcome::Refused { refusal } = outcome else {
                panic!("collision must refuse the transaction: {outcome:?}");
            };
            assert_namespace_collision(&refusal);
            assert_eq!(config.revision().await?, revision);
        }
        let report = session
            .send(crate::TurnInput::text("read the unchanged binding"))
            .output()
            .await?;
        assert!(
            matches!(report.result.outcome,
            crate::TurnOutcome::Finished(crate::TurnFinish::Finished { value, .. }) if value == serde_json::json!(42)),
            "the refused tools leave the binding usable"
        );
        drop(session);
        core.shutdown().await?;
    }
    Ok(())
}

/// FIG-5824: an offered namespace cannot displace a binding or saved function.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_configuration_refuses_an_existing_namespace_binding() -> Result<()> {
    namespace_collision_law(false).await
}

/// FIG-5824: per-run authority obeys the same admission rule as sticky config.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_run_tool_access_refuses_an_existing_namespace_binding() -> Result<()> {
    namespace_collision_law(true).await
}

#[cfg(feature = "rlm")]
fn assert_namespace_collision(refusal: &lash_core::ConfigRefusal) {
    assert!(
        matches!(refusal.owner_refusal::<lash_core::CoreConfigRefusal>(),
        Some(lash_core::CoreConfigRefusal::ToolNamespaceCollision { root, binding })
            if root == "data" && binding == "data"),
        "{refusal:?}"
    );
}

#[cfg(feature = "rlm")]
struct ChangingDataTool(Arc<std::sync::atomic::AtomicBool>);

#[cfg(feature = "rlm")]
#[async_trait]
impl ToolProvider for ChangingDataTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        if self.0.load(Ordering::SeqCst) {
            DataTool.tool_manifests()
        } else {
            Vec::new()
        }
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        DataTool.resolve_contract(name)
    }
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        DataTool.execute(call).await
    }
}

#[cfg(feature = "rlm")]
async fn namespace_installation_law(provider_refresh: bool) -> Result<()> {
    let visible = Arc::new(std::sync::atomic::AtomicBool::new(!provider_refresh));
    let mut source = vec![];
    if !provider_refresh {
        source.push(typescript_block("await control.finish(0);"));
    }
    source.extend([
        typescript_block("const data = 41; await control.finish(0);"),
        typescript_block("await control.finish(data + 1);"),
    ]);
    let cells = Arc::new(StdMutex::new(std::collections::VecDeque::from(source)));
    let provider = crate::testing::TestProvider::builder()
        .kind("namespace-installation")
        .complete(move |_| {
            let text = cells
                .lock_recover()
                .pop_front()
                .expect("only accepted runs call the model");
            async move { Ok(text_response(&text)) }
        })
        .build()
        .into_handle();
    let core =
        explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
            .serve_test_llm_profile(provider, mock_llm_profile_spec())
            .tools(Arc::new(ChangingDataTool(Arc::clone(&visible))))
            .build(crate::testing::runtime_lease_owner())?;
    let id =
        SessionId::parse(format!("namespace-installation-{provider_refresh}")).expect("session id");
    core.session(id.clone())
        .create(crate::SessionCreation::root(
            lash_core::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await?;
    let session = core.session(id).open().await?;
    if !provider_refresh {
        session
            .send(crate::TurnInput::text("record the tools"))
            .output()
            .await?;
        session
            .admin()
            .tools()
            .set_membership("tool:read-data", false, "hide-data")
            .await?
            .settle_with(
                &session.admin().commands(),
                crate::testing::admin_fixture_outcome,
            )
            .await?;
    }
    session
        .send(crate::TurnInput::text("bind data"))
        .output()
        .await?;
    let generation = session
        .admin()
        .tools()
        .state()
        .await?
        .recorded()
        .expect("recorded tools")
        .generation;
    let commands = session.admin().commands();
    if provider_refresh {
        visible.store(true, Ordering::SeqCst);
        let receipt = commands
            .refresh_tool_catalog("offer data", "offer-data")
            .await?;
        let lash_core::runtime::SessionCommandSettlement::Applied {
            outcome: lash_core::runtime::SessionCommandOutcome::Failed { refusal },
            ..
        } = commands.settle(receipt).await?
        else {
            panic!("the invalid provider installation settles refused");
        };
        let refusal = lash_core::RuntimeError::from(refusal);
        let Some(lash_core::RunShapeRefusal::Owner { refusal }) = refusal.run_shape_refusal()
        else {
            panic!("the command retains the typed collision: {refusal:?}");
        };
        assert_namespace_collision(refusal);
        // The provider belongs to the host: withdraw its rejected advertisement.
        visible.store(false, Ordering::SeqCst);
    } else {
        let mutation = session
            .admin()
            .tools()
            .set_membership("tool:read-data", true, "offer-data")
            .await?;
        let error = mutation
            .settle_with(&commands, crate::testing::admin_fixture_outcome)
            .await
            .expect_err("the membership addition conflicts with data");
        assert!(matches!(error, crate::EmbedError::Reconfigure(
            crate::tools::ReconfigureError::ToolNamespaceCollision { root, binding }
        ) if root == "data" && binding == "data"));
    }
    assert_eq!(
        session
            .admin()
            .tools()
            .state()
            .await?
            .recorded()
            .expect("recorded tools")
            .generation,
        generation
    );
    let report = session
        .send(crate::TurnInput::text("read data"))
        .output()
        .await?;
    assert!(matches!(report.result.outcome,
        crate::TurnOutcome::Finished(crate::TurnFinish::Finished { value, .. }) if value == serde_json::json!(42)));
    drop(session);
    core.shutdown().await?;
    Ok(())
}

/// FIG-5824: adding a previously absent provider namespace refuses at installation.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_installation_refuses_an_existing_namespace_binding() -> Result<()> {
    namespace_installation_law(true).await
}

/// FIG-5824: re-enabling a curated tool cannot displace a persisted binding.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_membership_refuses_an_existing_namespace_binding() -> Result<()> {
    namespace_installation_law(false).await
}
