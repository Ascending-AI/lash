//! Agent scenarios whose process bodies call the model, through a host's
//! `send()` on the core's node over SQLite memory stores (FIG-5307; the
//! scenarios FIG-5190 deleted with the engine double).

use super::*;

use crate::support::TurnInput;
use std::collections::VecDeque;
use tokio::sync::Mutex as TokioMutex;

/// A provider answering `responses` in order, recording every request.
fn scripted(responses: Vec<String>, requests: Arc<StdMutex<Vec<LlmRequest>>>) -> ProviderHandle {
    let responses = Arc::new(TokioMutex::new(VecDeque::from(responses)));
    crate::testing::TestProvider::builder()
        .kind("agent-scenario")
        .complete(move |request| {
            let responses = Arc::clone(&responses);
            let requests = Arc::clone(&requests);
            async move {
                requests.lock_recover().push(request.clone());
                let text = responses.lock().await.pop_front().ok_or_else(|| {
                    lash_core::llm::transport::LlmTransportError::new(
                        "scripted agent scenario provider exhausted its expected responses",
                    )
                })?;
                Ok(text_response(&text))
            }
        })
        .build()
        .into_handle()
}

/// An RLM core serving `provider`, with the session process controls.
fn scenario_core(
    backend: lash_core::Backend,
    provider: ProviderHandle,
) -> crate::core::LashCoreBuilder {
    explicit_ephemeral_facets(rlm_core_builder_over(backend))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        ))
}

/// A process body's `llm.query` with a typed output runs as one structured,
/// non-streaming model call, and the process answers its decoded value.
/// Each `llm.query` names its real owner on its provider request (ADR
/// 0022, FIG-5441): the cell's call its session, the process body's call
/// its process, never an id lash made up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_scenario_process_llm_query_with_typed_output() -> Result<()> {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core = scenario_core(
        sqlite_memory_store_backend().await,
        scripted(
            vec![
                typescript_block(
                    r#"
const label = await llm.query({ task: "Name the event", output: { name: "str" } });
const enrich = async (event) => {
  const enriched = await llm.query({
    task: "Classify the supplied email",
    inputs: { event: event },
    output: { category: "str", confidence: "float" }
  });
  return enriched;
};
const handle = await processes.start({ definition: enrich, args: { event: { email: "hello@example.com" } } });
finish(await handle);"#,
                ),
                r#"{"kind":"value","value":{"name":"email"},"error":null}"#.to_owned(),
                r#"{"kind":"value","value":{"category":"personal","confidence":0.98},"error":null}"#
                    .to_owned(),
            ],
            Arc::clone(&requests),
        ),
    )
    .plugin(Arc::new(lash_llm_tools::LlmToolsPluginFactory::default()))
    .build(crate::testing::runtime_lease_owner())?;
    let session_id = crate::SessionId::parse("agent-scenario-process-llm-query")
        .expect("nonblank host identity");
    let session = core
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?;
    let result = session
        .send(TurnInput::text("Enrich the email in a durable process."))
        .output()
        .await?;
    assert_eq!(
        result.final_value(),
        Some(&serde_json::json!({ "category": "personal", "confidence": 0.98 }))
    );
    let requests = requests.lock_recover().clone();
    assert_eq!(requests.len(), 3, "outer turn plus two llm_query calls");
    assert!(requests[2].stream_events.is_none());
    assert!(matches!(
        requests[2].output_spec,
        Some(lash_core::llm::types::LlmOutputSpec::JsonSchema(_))
    ));
    assert_eq!(
        requests[1].scope.owner,
        lash_core::LlmRequestOwner::Session { session_id },
        "the cell's llm.query is its session's"
    );
    let processes = core
        .backend()
        .process_registry()
        .list_processes(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        })
        .await?;
    let [process] = processes.as_slice() else {
        panic!("one process ran: {processes:?}");
    };
    assert_eq!(
        requests[2].scope.owner,
        lash_core::LlmRequestOwner::Process {
            process_id: process.id.clone()
        },
        "the process body's llm.query is its process's"
    );
    core.shutdown().await?;
    Ok(())
}

/// A repeatable tool whose body calls a direct completion and fails its
/// first attempt: the retry runs the whole attempt again, so the provider
/// is asked once per attempt, each under its own attempt number.
struct RetryingDirectTools;

fn retrying_direct_tool_definition() -> lash_core::ToolDefinition {
    use lash_core::ToolDefinitionBindingExt as _;
    lash_core::ToolDefinition::raw(
        "tool:retrying_direct",
        "retrying_direct",
        "Call a direct completion and retry the complete attempt once.",
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        serde_json::json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_execution_policy(lash_core::ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(2).expect("nonzero attempt bound"),
        0,
        0,
    ))
    .with_tool_binding(lash_vm_runtime::ToolBinding::new(
        ["tools"],
        "retrying_direct",
    ))
}

#[async_trait]
impl ToolProvider for RetryingDirectTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![retrying_direct_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "retrying_direct").then(|| Arc::new(retrying_direct_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let completion = match call
            .context
            .direct_completions()
            .complete(
                lash_core::facade_support::DirectRequest::text(format!(
                    "retrying direct completion attempt {}",
                    call.context.attempt_number()
                )),
                "retrying_direct",
            )
            .await
        {
            Ok(completion) => completion,
            Err(err) => return lash_core::ToolOutcome::err_fmt(err).into(),
        };
        if call.context.attempt_number() == 1 {
            return lash_core::ToolOutcome::failure(lash_core::ToolFailure::with_suggested_delay(
                lash_core::ToolFailureClass::Execution,
                "retrying_direct_first_attempt",
                "retry the complete atomic attempt",
                Some(0),
            ))
            .into();
        }
        lash_core::ToolOutcome::ok(serde_json::json!(completion.text)).into()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_scenario_direct_completion_attempt_retry_reinvokes_provider_once() -> Result<()> {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let core = scenario_core(
        sqlite_memory_store_backend().await,
        scripted(
            vec![
                typescript_block(
                    r#"
const retryDirect = async () => {
  const value = await tools.retrying_direct({});
  return value;
};
const handle = await processes.start({ definition: retryDirect });
finish(await handle);"#,
                ),
                "first-provider-result".to_owned(),
                "second-provider-result".to_owned(),
            ],
            Arc::clone(&requests),
        ),
    )
    .tools(Arc::new(RetryingDirectTools))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(
            crate::SessionId::parse("agent-scenario-direct-completion-attempt-retry")
                .expect("nonblank host identity"),
        )
        .created()
        .await
        .open()
        .await?;
    let result = session
        .send(TurnInput::text(
            "Retry the complete atomic tool attempt once.",
        ))
        .output()
        .await?;
    assert_eq!(
        result.final_value(),
        Some(&serde_json::json!("second-provider-result"))
    );
    let requests = requests.lock_recover().clone();
    assert_eq!(
        requests.len(),
        3,
        "outer turn plus one provider call for each of two tool attempts"
    );
    assert!(format!("{:?}", requests[1].messages).contains("attempt 1"));
    assert!(format!("{:?}", requests[2].messages).contains("attempt 2"));
    core.shutdown().await?;
    Ok(())
}
