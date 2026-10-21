//! Agent scenarios whose process bodies call the model, through a host's
//! `send()` on the core's node over SQLite memory stores (FIG-5307; the
//! scenarios FIG-5190 deleted with the engine double).

use super::*;

use lash::TurnInput;
use std::collections::VecDeque;
use tokio::sync::Mutex as TokioMutex;

/// A provider answering `responses` in order, recording every request.
fn scripted(responses: Vec<String>, requests: Arc<StdMutex<Vec<LlmRequest>>>) -> ProviderHandle {
    let responses = Arc::new(TokioMutex::new(VecDeque::from(responses)));
    lash::testing::TestProvider::builder()
        .kind("agent-scenario")
        .complete(move |request| {
            let responses = Arc::clone(&responses);
            let requests = Arc::clone(&requests);
            async move {
                requests.lock_recover().push(request.clone());
                let text = responses.lock().await.pop_front().ok_or_else(|| {
                    lash::provider::LlmTransportError::new(
                        "scripted agent scenario provider exhausted its expected responses",
                    )
                })?;
                Ok(text_response(&text))
            }
        })
        .build()
        .into_handle()
}

/// An RLM core serving `provider`, with the session process controls: a
/// process a cell starts runs under the turn's run with `components`.
fn scenario_core(
    backend: lash_execution_contracts::backend::Backend,
    provider: ProviderHandle,
    components: &[lash_core::ProcessConfigKey<()>],
) -> lash::LashCoreBuilder {
    explicit_ephemeral_facets(rlm_core_builder_over(backend))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                lash_execution_contracts::process::lifetime::session_or_starter,
                lash_core::testing::run_policy_naming(components),
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
await control.finish(await handle);"#,
                ),
                r#"{"kind":"value","value":{"name":"email"},"error":null}"#.to_owned(),
                r#"{"kind":"value","value":{"category":"personal","confidence":0.98},"error":null}"#
                    .to_owned(),
            ],
            Arc::clone(&requests),
        ),
        &[lash_llm_tools::LLM_TOOLS_CONFIG],
    )
    .plugin(Arc::new(lash_llm_tools::LlmToolsPluginFactory::default()))
    .process_component(
        Arc::new(lash_llm_tools::LlmToolsComponent::default()),
        lash_llm_tools::LLM_TOOLS_CONFIG,
    )
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session_id =
        lash_vocabulary::identity::SessionId::parse("agent-scenario-process-llm-query")
            .expect("nonblank host identity");
    let session = created_session(&core, session_id.clone())
        .await
        .open()
        .await?;
    let result = session
        .send(TurnInput::text("Enrich the email in a durable process."))
        .output()
        .await?;
    assert_eq!(
        result.finished().map(|(_, value)| value),
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
    let processes = lash_core::testing::process_roster_records_for_fixture(
        core.backend().process_registry().as_ref(),
        &lash_execution_contracts::process::ProcessListFilter {
            status: lash_execution_contracts::process::ProcessStatusFilter::Any,
            ..lash_execution_contracts::process::ProcessListFilter::default()
        },
    )
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

fn retrying_direct_tool_definition() -> lash_vocabulary::tool_contract::ToolDefinition {
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        "tool:retrying_direct",
        "tools.retrying_direct",
        "Call a direct completion and retry the complete attempt once.",
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        serde_json::json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_execution_policy(lash_vocabulary::tool_contract::ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(2).expect("nonzero attempt bound"),
        0,
        0,
    ))
}

/// The component that serves `retrying_direct` to a process body.
const RETRYING_DIRECT_COMPONENT: lash_core::ProcessConfigKey<()> = lash_core::ProcessConfigKey::new(
    "retrying-direct",
    lash_vocabulary::providers::FormatVersion::ONE,
);

#[async_trait]
impl ToolProvider for RetryingDirectTools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash::plugins::PluginError,
    > {
        Ok(vec![retrying_direct_tool_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        (name == "tools.retrying_direct")
            .then(|| Arc::new(retrying_direct_tool_definition().contract()))
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
  const value = await tools.retryingDirect({});
  return value;
};
const handle = await processes.start({ definition: retryDirect });
await control.finish(await handle);"#,
                ),
                "first-provider-result".to_owned(),
                "second-provider-result".to_owned(),
            ],
            Arc::clone(&requests),
        ),
        &[RETRYING_DIRECT_COMPONENT],
    )
    .plugin(lash_core::testing::tool_plugin(
        RETRYING_DIRECT_COMPONENT,
        Arc::new(RetryingDirectTools),
    ))
    .process_component(
        lash_core::testing::tool_component(
            RETRYING_DIRECT_COMPONENT,
            Arc::new(RetryingDirectTools),
        ),
        RETRYING_DIRECT_COMPONENT,
    )
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash_vocabulary::identity::SessionId::parse(
            "agent-scenario-direct-completion-attempt-retry",
        )
        .expect("nonblank host identity"),
    )
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
        result.finished().map(|(_, value)| value),
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

/// The oversized result crosses the ordinary tool seam; the provider's
/// overflow is scripted because a model window cannot be shrunk by a fixture.
struct OverflowPayloadTools(Arc<AtomicUsize>);

fn overflow_payload_definition() -> lash_vocabulary::tool_contract::ToolDefinition {
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        "tool:overflow_payload",
        "tools.overflow_payload",
        "Return the oversized report.",
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        serde_json::json!({ "type": "string" }),
    )
    .expect("valid tool schemas")
    .with_execution(std::time::Duration::from_secs(30))
}

#[async_trait]
impl ToolProvider for OverflowPayloadTools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash::plugins::PluginError,
    > {
        Ok(vec![overflow_payload_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        (name == "tools.overflow_payload")
            .then(|| Arc::new(overflow_payload_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        lash_core::ToolOutcome::ok(serde_json::json!("x".repeat(256 * 1024))).into()
    }
}

/// FIG-5986: an RLM session that overflowed after a tool result accepts a
/// subsequent turn on the same durable identity, retaining its committed
/// binding. Both an explicit overflow and a classified failed call obey it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_overflow_mid_turn_continues_on_the_same_session() -> Result<()> {
    for classified in [false, true] {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let provider = lash::testing::TestProvider::builder()
            .kind("context-overflow-continuation")
            .complete({
                let requests = Arc::clone(&requests);
                let tool_calls = Arc::clone(&tool_calls);
                move |request| {
                    let call = {
                        let mut requests = requests.lock_recover();
                        requests.push(request.clone());
                        requests.len()
                    };
                    let tool_calls = Arc::clone(&tool_calls);
                    async move {
                        match call {
                            1 => Ok(text_response(&typescript_block(
                                "const overflow_payload = await tools.overflowPayload({});\nconsole.log(overflow_payload.length);",
                            ))),
                            2 => {
                                assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
                                assert!(request_text(&request).contains("262144"));
                                if classified {
                                    Err(lash::provider::LlmTransportError::new(
                                        "This model's maximum context length is 128000 tokens",
                                    )
                                    .with_http_status(400))
                                } else {
                                    Ok(LlmResponse {
                                        terminal_reason: lash_core::LlmTerminalReason::ContextOverflow,
                                        terminal_diagnostic: Some("context window exceeded".into()),
                                        ..Default::default()
                                    })
                                }
                            }
                            3 => Ok(text_response(&typescript_block(
                                "await control.finish(overflow_payload.length);",
                            ))),
                            _ => panic!("unexpected provider call {call}"),
                        }
                    }
                }
            })
            .build()
            .into_handle();
        let core =
            explicit_ephemeral_facets(rlm_core_builder_over(sqlite_memory_store_backend().await))
                .serve_test_llm_profile(provider, mock_llm_profile_spec())
                .tools(
                    lash::tool_provider_key!("tools"),
                    Arc::new(OverflowPayloadTools(Arc::clone(&tool_calls))),
                )
                .build(lash_facade_tests::runtime_lease_owner())?;
        let session_id = lash_vocabulary::identity::SessionId::parse(format!(
            "overflow-continuation-{classified}"
        ))
        .expect("nonblank session identity");
        let session = created_session(&core, session_id.clone())
            .await
            .open()
            .await?;
        let overflow = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            session
                .send(TurnInput::text("Read the oversized report."))
                .output(),
        )
        .await
        .expect("the overflow turn settles")?;
        assert!(
            overflow.outcome.is_context_overflow(),
            "{:?}",
            overflow.outcome
        );
        assert!(!overflow.outcome.is_success());
        assert_eq!(committed(&session).await.turn_index(), 1);
        drop(session);

        // Open the existing session, never create a replacement. The answer
        // reads the binding committed before the overflow, without rerunning
        // the tool or restoring anything in host code.
        let session = core.session(session_id.clone()).open().await?;
        let continued = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            session
                .send(TurnInput::text("Continue from the report."))
                .output(),
        )
        .await
        .expect("the continuing turn settles")?;
        assert!(continued.outcome.is_success(), "{:?}", continued.outcome);
        assert!(!continued.outcome.is_context_overflow());
        assert_eq!(
            continued.finished().map(|(_, value)| value),
            Some(&serde_json::json!(262144))
        );
        assert_eq!(committed(&session).await.turn_index(), 2);
        assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
        let requests = requests.lock_recover().clone();
        assert_eq!(
            requests.len(),
            3,
            "two calls before overflow, one to continue"
        );
        assert!(
            requests
                .iter()
                .all(|request| request.session_id() == Some(&session_id))
        );
        drop(session);
        core.shutdown().await?;
    }
    Ok(())
}
