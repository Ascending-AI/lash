//! A tool's success settles against the output schema of the contract its
//! call was prepared under (FIG-6295), by the rule a code cell decodes the
//! value by: a value the schema does not admit fails the call, naming the
//! tool and where the value does not fit, so the activity event, the host
//! and the model all see the one failed outcome.

use super::*;

use crate::{TurnEvent, TurnInput};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::facade_support::{TurnFinish, TurnOutcome};

const SEARCH: &str = "search";

/// `search`: a host tool whose result states a `diagnostics` record with a
/// required `took`.
fn search_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{SEARCH}"),
        SEARCH,
        "Search the index.",
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "hits": { "type": "array", "items": { "type": "string" } },
                "diagnostics": {
                    "type": "object",
                    "properties": { "took": { "type": "integer" } },
                    "required": ["took"],
                    "additionalProperties": false
                }
            },
            "required": ["hits", "diagnostics"],
            "additionalProperties": false
        }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(30))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], SEARCH))
}

/// `search`, answering a `diagnostics` that lacks its required `took`.
struct SearchTool;

#[async_trait]
impl ToolProvider for SearchTool {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, crate::plugins::PluginError> {
        Ok(vec![search_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        (name == SEARCH).then(|| Arc::new(search_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!({ "hits": [], "diagnostics": {} })).into()
    }
}

/// FIG-6295 law 1: a host tool that answers a value missing a required
/// output field settles as a typed failure naming the tool and the field's
/// path. `ToolCallCompleted` reports the failure, and the model's next
/// request reads it with the tool's name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_success_its_output_schema_refuses_settles_as_a_failure_naming_the_tool() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
    let provider = {
        let (calls, requests) = (Arc::clone(&calls), Arc::clone(&requests));
        crate::testing::TestProvider::builder()
            .kind("output-settlement")
            .complete(move |request| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                requests
                    .lock_recover()
                    .push(format!("{:?}", request.messages));
                async move {
                    if call == 0 {
                        return Ok(LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "call-search".to_string(),
                                tool_name: SEARCH.to_string(),
                                input_json: "{}".to_string(),
                                replay: None,
                            }],
                            ..LlmResponse::default()
                        });
                    }
                    Ok(text_response("done"))
                }
            })
            .build()
            .into_handle()
    };
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .tools(crate::tool_provider_key!("tools"), Arc::new(SearchTool))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(SessionId::parse("output-settlement").expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let events = RecordingEvents::default();
    let report = session
        .send(TurnInput::text("search the index"))
        .output_into(&events)
        .await?;
    let completed: Vec<_> = events
        .snapshot()
        .await
        .into_iter()
        .filter_map(|activity| match activity.event {
            TurnEvent::ToolCallCompleted { output, .. } => Some(output),
            _ => None,
        })
        .collect();
    drop(session);
    core.shutdown().await?;

    let [output] = completed.as_slice() else {
        panic!("one settled call: {completed:?}");
    };
    let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome else {
        panic!("the call fails: {output:?}");
    };
    let Some(lash_core::ToolFailureCause::Declaration {
        refusal: lash_core::DeclarationRefusal::OutputValueMismatch { mismatch, .. },
    }) = failure.cause.as_deref()
    else {
        panic!("the failure is the typed output mismatch: {failure:?}");
    };
    assert_eq!(mismatch.instance_path, "/diagnostics", "{mismatch:?}");
    assert!(mismatch.message.contains("`took`"), "{mismatch:?}");
    assert!(
        failure.message.contains("tool `search`"),
        "the failure names the tool: {failure:?}"
    );
    assert_eq!(
        report.outcome,
        TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: "done".to_string(),
        }),
        "the turn goes on to the model's answer"
    );
    let requests = requests.lock_recover().clone();
    let [_, next] = requests.as_slice() else {
        panic!("two model calls: {requests:?}");
    };
    assert!(
        next.contains("tool `search`") && next.contains("/diagnostics"),
        "the model reads the failure with the tool's name and the path: {next}"
    );
    Ok(())
}
