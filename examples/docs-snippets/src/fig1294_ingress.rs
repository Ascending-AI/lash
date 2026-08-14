use std::sync::Arc;

use async_trait::async_trait;
use lash::process::ProcessRegistry as _;
use lash::tools::{
    StaticToolExecute, StaticToolProvider, ToolCall, ToolDefinition, ToolProvider, ToolResult,
};

const SESSION: &str = "docs-ingress-session";
const SCOPE: &str = "docs-ingress-turn";
const PROCESS: &str = "docs-ingress-process";
const EVENT: &str = "docs.ingress.event";

fn test_core(
    registry: Arc<lash::testing::TestLocalProcessRegistry>,
) -> lash::Result<lash::LashCore> {
    lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .provider(lash::provider::ProviderHandle::unconfigured())
        .model(
            lash::ModelSpec::builder("docs-ingress-model")
                .context_window_tokens(4_096)
                .build()
                .expect("valid docs ingress model"),
        )
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(registry)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build()
}

fn event_intent(session_id: &str) -> lash::tools::ToolIntent {
    lash::tools::ToolIntent::EmitProcessEvent(lash::tools::EmitProcessEventIntent {
        session_id: session_id.to_string(),
        process_id: PROCESS.to_string(),
        event_type: EVENT.to_string(),
        payload: serde_json::json!({"source": "docs-snippet"}),
    })
}

#[tokio::test]
async fn host_ingress_has_typed_identity_dedupe_and_refusals() -> anyhow::Result<()> {
    let registry = Arc::new(lash::testing::TestLocalProcessRegistry::default());
    registry
        .register_process_with_observers(
            lash::process::ProcessRegistration::new(
                PROCESS,
                lash::process::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash::process::RecoveryDisposition::ExternallyOwned,
                lash::process::ProcessProvenance::host(),
            )
            .with_extra_event_types([lash::process::ProcessEventType {
                name: EVENT.to_string(),
                payload_schema: lash::triggers::LashSchema::any(),
                semantics: lash::process::ProcessEventSemanticsSpec::default(),
            }]),
            &[SESSION.to_string()],
        )
        .await?;
    let core = test_core(Arc::clone(&registry))?;
    let _session = core.session(SESSION).open().await?;
    let ingress: lash::tools::ToolIntentIngress =
        core.tool_intents(SESSION, lash::runtime::ExecutionScope::turn(SESSION, SCOPE))?;
    let key: lash::tools::ToolIntentIngressKey = ingress.key("docs-call", 0);
    let derived = lash::tools::ToolIntentIngressKey::derive(SESSION, SCOPE, "docs-call", 0);
    assert_eq!(key.identity(), derived.identity());

    let admitted = ingress.submit(key.clone(), event_intent(SESSION)).await;
    let duplicate = ingress.submit(key, event_intent(SESSION)).await;
    let lash::tools::ToolIntentIngressOutcome::Admitted { outcome } = admitted else {
        panic!("the valid host intent must be admitted")
    };
    assert!(matches!(
        outcome,
        lash::tools::ToolIntentExecutionOutcome::Executed { .. }
    ));
    let expected_duplicate = lash::tools::ToolIntentIngressOutcome::Admitted { outcome };
    assert_eq!(duplicate, expected_duplicate);

    let foreign_session = ingress
        .submit(
            lash::tools::ToolIntentIngressKey::derive("foreign", SCOPE, "docs-call", 1),
            event_intent(SESSION),
        )
        .await;
    let lash::tools::ToolIntentIngressOutcome::Refused { refusal } = foreign_session else {
        panic!("foreign session must be refused")
    };
    let lash::tools::ToolIntentIngressRefusal::ForeignSession { expected, recorded } = refusal
    else {
        panic!("foreign session refusal must retain both values")
    };
    assert_eq!((expected.as_str(), recorded.as_str()), (SESSION, "foreign"));

    let foreign_scope = ingress
        .submit(
            lash::tools::ToolIntentIngressKey::derive(SESSION, "foreign-scope", "docs-call", 2),
            event_intent(SESSION),
        )
        .await;
    let lash::tools::ToolIntentIngressOutcome::Refused {
        refusal: lash::tools::ToolIntentIngressRefusal::ForeignExecutionScope { expected, recorded },
    } = foreign_scope
    else {
        panic!("foreign scope must be refused")
    };
    assert_eq!(
        (expected.as_str(), recorded.as_str()),
        (SCOPE, "foreign-scope")
    );

    let intent_session = ingress
        .submit(ingress.key("docs-call", 3), event_intent("foreign-intent"))
        .await;
    let lash::tools::ToolIntentIngressOutcome::Refused {
        refusal: lash::tools::ToolIntentIngressRefusal::IntentSessionMismatch { expected, recorded },
    } = intent_session
    else {
        panic!("foreign intent session must be refused")
    };
    assert_eq!(
        (expected.as_str(), recorded.as_str()),
        (SESSION, "foreign-intent")
    );

    let mut forged = derived.identity().clone();
    forged.replay_key = "forged".to_string();
    let malformed = ingress
        .submit(
            lash::tools::ToolIntentIngressKey::from_identity(forged),
            event_intent(SESSION),
        )
        .await;
    let lash::tools::ToolIntentIngressOutcome::Refused {
        refusal:
            lash::tools::ToolIntentIngressRefusal::MalformedKey {
                expected_replay_key,
                recorded_replay_key,
            },
    } = malformed
    else {
        panic!("forged replay key must be refused")
    };
    assert!(expected_replay_key.starts_with("tool-intent:v1:sha256:"));
    assert_eq!(recorded_replay_key, "forged");
    Ok(())
}

struct InternalProbe;

#[async_trait]
impl StaticToolExecute for InternalProbe {
    async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
        ToolResult::ok(serde_json::json!({"fallback": call.name}))
    }

    async fn execute_internal(&self, call: lash_core::InternalProcessToolCall<'_>) -> ToolResult {
        let lash_core::InternalProcessToolCall {
            name,
            args,
            context,
        } = call;
        let admin: lash_core::InternalProcessAdmin<'_> = context.processes();
        let start = admin
            .start(lash_core::ProcessStartRequest::external(
                "docs-internal-start",
                lash_core::ProcessOriginator::host(),
                serde_json::Value::Null,
            ))
            .await;
        let list = admin
            .list_handles_filtered(&lash_core::ProcessListFilter::default())
            .await;
        let await_process = admin.await_process("docs-missing").await;
        let cancel = admin.cancel("docs-missing").await;
        let signal = admin
            .signal("docs-missing", "resume", serde_json::Value::Null)
            .await;
        let complete = admin
            .complete_external(
                "docs-missing",
                lash_core::ProcessAwaitOutput::Success {
                    value: serde_json::Value::Null,
                    control: None,
                },
            )
            .await;
        let _events = context.process_events();
        ToolResult::ok(serde_json::json!({
            "name": name,
            "args": args,
            "session_id": context.session_id(),
            "process_id": context.process_id(),
            "has_cancellation": context.cancellation_token().is_some(),
            "start_ok": start.is_ok(),
            "list_ok": list.is_ok(),
            "await_error": await_process.is_err(),
            "cancel_error": cancel.is_err(),
            "signal_error": signal.is_err(),
            "complete_error": complete.is_err(),
        }))
    }
}

#[tokio::test]
async fn internal_process_contract_is_separate_and_observable() {
    let tool = lash_core::testing::mock_tool_context();
    let context = lash_core::InternalProcessContext::__for_testing(&tool);
    let definition = ToolDefinition::raw(
        "tool:internal_probe",
        "internal_probe",
        "exercise the internal process-engine context",
        ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object"}),
    );
    let tool_id = definition.manifest.id.clone();
    let provider = StaticToolProvider::new(vec![definition], InternalProbe);
    let result = lash::tools::ToolProvider::execute_internal_by_id(
        &provider,
        &tool_id,
        &serde_json::json!({"probe": true}),
        &context,
    )
    .await;
    let value = result.as_output().value_for_projection();
    assert_eq!(value["name"], "internal_probe");
    assert_eq!(value["args"], serde_json::json!({"probe": true}));
    assert_eq!(value["session_id"], "test-session");
    assert!(value["process_id"].is_null());
    assert_eq!(value["has_cancellation"], false);
    assert_eq!(value["start_ok"], false);
    assert_eq!(value["list_ok"], false);
    assert_eq!(value["await_error"], true);
    assert_eq!(value["cancel_error"], true);
    assert_eq!(value["signal_error"], true);
    assert_eq!(value["complete_error"], true);

    let direct = lash::tools::StaticToolExecute::execute_internal(
        provider.executor(),
        lash_core::InternalProcessToolCall {
            name: "internal_probe",
            args: &serde_json::json!({"direct": true}),
            context: &context,
        },
    )
    .await;
    assert_eq!(
        direct.as_output().value_for_projection()["name"],
        "internal_probe"
    );

    let fallback = lash::tools::ToolProvider::execute_internal(
        &provider,
        lash_core::InternalProcessToolCall {
            name: "internal_probe",
            args: &serde_json::json!({"fallback": true}),
            context: &context,
        },
    )
    .await;
    assert_eq!(
        fallback.as_output().value_for_projection()["name"],
        "internal_probe"
    );
}
