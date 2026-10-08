use super::*;

#[test]
fn host_llm_profile_capability_validates_reasoning_effort_selections() {
    use lash::provider::{
        LlmProfileEffortValidationCategory, LlmProfileEffortValidationError, ReasoningCapability,
        ReasoningEncoding, ReasoningSelection,
    };

    let capability = workbench_llm_profile_capability();
    let unsupported: LlmProfileEffortValidationError = capability
        .validate_selection(
            "workbench-model",
            "workbench-provider",
            &ReasoningSelection::Effort("ultra".to_string()),
        )
        .expect_err("host capability must reject an unadvertised effort");
    assert_eq!(
        unsupported.category,
        LlmProfileEffortValidationCategory::UnsupportedEffort
    );
    assert!(unsupported.message.contains("Unsupported effort `ultra`"));
    capability
        .validate_selection(
            "workbench-model",
            "workbench-provider",
            &ReasoningSelection::Effort("high".to_string()),
        )
        .expect("host capability accepts an advertised effort");
    assert_eq!(
        capability
            .validate_selection(
                "workbench-model",
                "workbench-provider",
                &ReasoningSelection::Effort(" HIGH ".to_string()),
            )
            .expect_err("effort names match exactly")
            .category,
        LlmProfileEffortValidationCategory::UnsupportedEffort
    );

    let not_configurable = lash::provider::LlmProfileCapability::default()
        .validate_selection(
            "plain-model",
            "workbench-provider",
            &ReasoningSelection::Effort("low".to_string()),
        )
        .expect_err("plain model must reject configurable effort");
    assert_eq!(
        not_configurable.category,
        LlmProfileEffortValidationCategory::EffortNotConfigurable
    );
    assert!(
        not_configurable
            .message
            .contains("does not expose configurable effort")
    );

    let mut required_capability = capability.clone();
    required_capability
        .reasoning
        .as_mut()
        .expect("workbench reasoning capability")
        .mandatory = true;
    let required = required_capability
        .validate_selection(
            "required-model",
            "workbench-provider",
            &ReasoningSelection::ProviderDefault,
        )
        .expect_err("mandatory reasoning must require an explicit effort");
    assert_eq!(
        required.category,
        LlmProfileEffortValidationCategory::EffortRequired
    );
    assert!(required.message.contains("requires an explicit effort"));

    let malformed_capability = lash::provider::LlmProfileCapability {
        reasoning: Some(ReasoningCapability {
            efforts: vec!["low".to_string(), "high".to_string()],
            encoding: ReasoningEncoding::Budget(BTreeMap::from([("low".to_string(), 1_024)])),
            disable: false,
            mandatory: false,
        }),
        ..Default::default()
    };
    let malformed = malformed_capability
        .validate_selection(
            "malformed-model",
            "workbench-provider",
            &ReasoningSelection::Effort("low".to_string()),
        )
        .expect_err("budget map must cover every advertised effort");
    assert_eq!(
        malformed.category,
        LlmProfileEffortValidationCategory::MalformedCapability
    );
    assert!(
        malformed
            .message
            .contains("missing advertised effort `high`")
    );
}

/// The workbench plugin observes a session's model change through its
/// config-change hook, after the change commits: it sees the previous and
/// the new model, and the session it reads already serves the new one. The
/// session actor delivers the change from the command run that applied it,
/// after `apply` returns, so the law runs a turn behind it in the same actor
/// and reads the observation once that turn has answered (FIG-5333).
#[tokio::test]
async fn workbench_plugin_observes_session_config_policy_transition() {
    let stores: Arc<dyn lash::StoreSet> = Arc::new(
        lash::sqlite::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    let backend = lash::durable::DurableBackendBuilder::new(stores)
        .build()
        .expect("the durable backend builds");
    let plugin = WorkbenchPluginFactory::new();
    let config_changes = plugin.config_changes.clone();
    let core = LashCore::standard_builder(backend)
        .llm_profiles(Arc::new(WorkbenchLlmProfiles {
            provider: replying_provider("done"),
        }))
        .plugin(Arc::new(plugin))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "agent-workbench-test",
            uuid::Uuid::new_v4().to_string(),
        ))
        .expect("build the config-change core");
    let session_id = lash::SessionId::from("workbench-config-change-session");
    core.session(session_id.clone())
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            lash::SessionSpec::new(
                "workbench-model-before",
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(8),
            )
            .no_progress_budget(lash::NoProgressBudget::bounded(12)),
        ))
        .await
        .expect("create the config-change session");
    let session = core
        .session(session_id.clone())
        .open()
        .await
        .expect("open the config-change session");
    let config = session.admin().config();
    let outcome = config
        .apply(
            lash::config::ConfigWrite::new(
                "workbench-model-after",
                config.revision().await.expect("read the config revision"),
            ),
            lash::config::ConfigTransaction::of(lash::config::SetLlmProfile {
                model: lash::LlmProfileKey::new("workbench-model-after"),
            }),
        )
        .await
        .expect("config accepted")
        .await_outcome(&config)
        .await
        .expect("patch the session's model");
    assert!(
        matches!(
            outcome,
            lash::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{outcome:?}"
    );
    // The session actor runs this turn after the command run that applied
    // the change: its answer means the observer has run.
    let turn = session
        .send(lash::TurnInput::text("after the change"))
        .output()
        .await
        .expect("the turn after the change answers");
    assert!(turn.is_success(), "{turn:?}");

    assert_eq!(
        config_changes.latest.lock_recover().clone(),
        Some(WorkbenchConfigChange {
            session_id: session_id.clone(),
            previous_profile_key: "workbench-model-before".to_string(),
            current_profile_key: "workbench-model-after".to_string(),
            service_profile_key: "workbench-model-after".to_string(),
        })
    );
    let reopened = core
        .session(session_id)
        .open()
        .await
        .expect("reopen the config-change session");
    assert_eq!(
        reopened
            .policy_snapshot()
            .profile_key()
            .map(ToString::to_string),
        Some("workbench-model-after".to_string())
    );
    drop(reopened);
    drop(session);
    core.shutdown().await.expect("the core shuts down");
}
