use super::*;
use lash_lashlang_runtime::ToolBinding;
use std::sync::atomic::{AtomicUsize, Ordering};

const TOOL: &str = "paid_completion";

fn completion_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:process_paid_completion",
        TOOL,
        "Make one managed direct completion for this process.",
        serde_json::json!({"type": "object", "additionalProperties": false}),
        serde_json::json!({"type": "string"}),
    )
    .with_tool_binding(ToolBinding::new(["tools"], TOOL))
}

struct CompletionTool;

#[async_trait::async_trait]
impl lash_core::ToolProvider for CompletionTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![completion_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == TOOL).then(|| Arc::new(completion_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let completion = call
            .context
            .direct_completions()
            .complete(
                crate::direct::DirectRequest::text("mock-model", "process paid call"),
                "process-direct",
            )
            .await
            .expect("the process's direct completion succeeds");
        lash_core::ToolOutcome::ok(serde_json::json!(completion.text)).into()
    }
}

/// E7: two direct provider calls made by a running Lashlang process are
/// charged to that process, and its prune drains accounting before retiring
/// its journal. No accounting rows are seeded by the fixture.
#[tokio::test]
async fn process_calls_are_owned_by_their_runtime_and_drained_at_prune() -> Result<()> {
    process_usage_and_prune(double_backend().await).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run through the usage-accounting service gate"]
#[expect(
    clippy::disallowed_methods,
    reason = "the PostgreSQL fixture reads the service gate URL"
)]
async fn process_calls_are_owned_by_their_runtime_and_drained_at_prune_on_postgres() -> Result<()> {
    let url = std::env::var("LASH_POSTGRES_DATABASE_URL").expect("PostgreSQL gate URL");
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::PostgresStorage::connect(database.url())
        .await
        .expect("connect PostgreSQL");
    let attachments = tempfile::tempdir().expect("attachment directory");
    let attachment_store = Arc::new(lash_core::facade_support::FileAttachmentStore::new(
        attachments.path(),
    ));
    let backend = double_backend_over(lash_restate_test::ServerConfig::default(), move |base| {
        Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
            &storage,
            attachment_store,
            Default::default(),
            base.clock(),
        ))
    })
    .await;
    process_usage_and_prune(backend).await
}

async fn process_usage_and_prune(backend: lash_core::Backend) -> Result<()> {
    persist_process_env_ref(backend.process_env_store().as_ref()).await;
    let invocations = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&invocations);
    let provider = crate::testing::TestProvider::builder()
        .kind("mock")
        .complete(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Ok(lash_core::LlmResponse {
                    parts: vec![lash_core::LlmOutputPart::Text {
                        text: "paid".into(),
                        response_meta: None,
                    }],
                    terminal_reason: lash_core::LlmTerminalReason::Stop,
                    usage: lash_core::llm::types::LlmUsage {
                        input_tokens: 30,
                        output_tokens: 6,
                        ..Default::default()
                    },
                    provider_usage: Some(serde_json::json!({"paid": true})),
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    let core = process_test_builder(backend.clone())
        .serve_test_model(provider, mock_model_spec())
        .tools(Arc::new(CompletionTool) as Arc<dyn lash_core::ToolProvider>)
        .build(crate::testing::runtime_lease_owner())?;
    serve_processes(&core);

    let contract = completion_definition().contract();
    let mut catalog = programs::process_control_catalog();
    catalog
        .add_module_operation_contract(
            ["tools"],
            "Tools",
            TOOL,
            "tool:process_paid_completion",
            &lashlang::OperationContract::new(
                contract.input_schema.canonical().clone(),
                contract.output_schema.canonical().clone(),
            ),
        )
        .expect("link the managed completion tool");
    let call = || b::module_call(&["tools"], TOOL, vec![b::record(Vec::new())]);
    let program = b::module(
        vec![b::process(
            "main",
            Vec::new(),
            b::block(vec![
                b::assign("first", call()),
                b::assign("second", call()),
                b::finish(b::var("second")),
            ]),
        )],
        Vec::new(),
    );
    let process = LinkedTestProcess::new_with_catalog(
        &lash_lashlang_runtime::LashlangArtifacts::of_backend(&backend),
        program,
        "main",
        catalog,
    )
    .await;
    let env = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy {
            model: Some(recorded_model(mock_model_spec())),
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        },
    );
    let env_ref = lash_core::publish_process_execution_env(
        backend.process_env_store().as_ref(),
        &lash_core::testing::host_pin_claim_for_testing(),
        &env,
    )
    .await?;
    let id = core
        .processes()
        .start(
            process
                .start_request("process-usage-two-calls")
                .with_env_ref(env_ref),
            runtime_operation_scope(&core, "process-usage-start").await,
        )
        .await?
        .process_id;
    wait_for_terminal(&core, &id, lash_core::ProcessStatus::Completed).await;
    assert_eq!(invocations.load(Ordering::SeqCst), 2, "two paid calls");

    let owner = lash_core::RuntimeOwner::Process(id.clone());
    let report = core
        .processes()
        .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await?;
    assert_eq!(report.pruned_processes, 1);
    let usage = core.owner_usage(&owner).await?;
    assert!(usage.completeness.retired, "the prune retired the owner");
    assert_eq!(usage.completeness.open_runs, 0);
    assert_eq!(usage.completeness.unknown_runs, 0);
    assert_eq!(usage.completeness.conflicted_runs, 0);
    assert_eq!(usage.completeness.unreported_attempts, 0);
    let [row] = usage.rows.as_slice() else {
        panic!("the process's two paid calls survive prune: {usage:?}");
    };
    assert_eq!(row.source, "process-direct");
    assert_eq!(row.reported_attempts, 2);
    assert_eq!(row.usage.input_tokens, 60);
    assert_eq!(row.usage.output_tokens, 12);
    let facts = core
        .usage_fact_page(
            &owner,
            None,
            std::num::NonZeroU32::new(10).expect("nonzero"),
        )
        .await?;
    assert_eq!(facts.facts.len(), 2);
    assert!(facts.facts.iter().all(|fact| fact.identity.owner == owner));
    assert_ne!(
        facts.facts[0].identity.effect,
        facts.facts[1].identity.effect
    );

    let scope = lash_core::ExecutionScope::process(id);
    let refused = backend
        .usage_accounting()
        .admit_usage_run(&lash_core::UsageRunAdmission {
            owner,
            effect: lash_core::UsageEffectKey::for_effect(
                &lash_sansio::EffectAddress::new(scope.clone(), "after-prune")
                    .expect("process effect address"),
            ),
            execution_scope_key: scope
                .journal_identity()
                .expect("process identity")
                .key()
                .into(),
            run: lash_core::UsageRunId::mint(),
            source: "process-direct".into(),
            model: "mock-model".into(),
            admitted_at_ms: 1,
        })
        .await;
    assert!(matches!(
        refused,
        Err(lash_core::UsageAdmissionError::OwnerRetired { .. })
    ));
    assert_eq!(invocations.load(Ordering::SeqCst), 2, "prune buys nothing");
    Ok(())
}
