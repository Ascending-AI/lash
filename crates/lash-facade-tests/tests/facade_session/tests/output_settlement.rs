//! A tool's success settles against the output schema of the contract its
//! call was prepared under (FIG-6295), admitted by the whole declared
//! schema (FIG-6330): a value the schema does not admit fails the call,
//! naming the tool and where the value does not fit, so the activity event,
//! the host and the model all see the one failed outcome.

use super::*;

use lash::{TurnEvent, TurnInput};
use lash_core::facade_support::{TurnFinish, TurnOutcome};

const SEARCH: &str = "search";

const SEARCH_OPERATION: &str = "tools.search";

/// `search`: a host tool whose result states a `diagnostics` record with a
/// required `took`.
fn search_definition() -> lash_vocabulary::tool_contract::ToolDefinition {
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        format!("tool:{SEARCH}"),
        SEARCH_OPERATION,
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
}

/// `search`, answering a `diagnostics` that lacks its required `took`.
struct SearchTool;

#[async_trait]
impl ToolProvider for SearchTool {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash::plugins::PluginError,
    > {
        Ok(vec![search_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        (name == SEARCH_OPERATION).then(|| Arc::new(search_definition().contract()))
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
        lash::testing::TestProvider::builder()
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
                                tool_name: SEARCH_OPERATION.replace('.', "__"),
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
    .tools(lash::tool_provider_key!("tools"), Arc::new(SearchTool))
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session = core
        .session(SessionId::parse("output-settlement").expect("nonblank host identity"))
        .create(lash::SessionCreation::root(mock_session_spec()))
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
        refusal:
            lash_vocabulary::tool_declaration::DeclarationRefusal::OutputValueMismatch {
                mismatch, ..
            },
    }) = failure.cause.as_deref()
    else {
        panic!("the failure is the typed output mismatch: {failure:?}");
    };
    assert_eq!(mismatch.instance_path, "/diagnostics", "{mismatch:?}");
    assert!(mismatch.message.contains("\"took\""), "{mismatch:?}");
    assert!(
        failure.message.contains("tool `tools.search`"),
        "the failure names the tool: {failure:?}"
    );
    assert_eq!(
        report,
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
        next.contains("tool `tools.search`") && next.contains("/diagnostics"),
        "the model reads the failure with the tool's name and the path: {next}"
    );
    Ok(())
}

/// FIG-6334: the committed call body is the host's settle verdict, including
/// a refusal of the host's Success. A cold durable reader follows a cell's
/// call id and reads the failure code, message and typed cause from that body.
#[cfg(feature = "rlm")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_reads_the_schema_refusal_from_the_cells_committed_call() -> Result<()> {
    let responses = Arc::new(AtomicUsize::new(0));
    let provider = lash::testing::TestProvider::builder()
        .kind("settle-verdict")
        .complete(move |_| {
            let response = responses.fetch_add(1, Ordering::SeqCst);
            async move {
                let text = if response == 0 {
                    typescript_block("await tools.search({});")
                } else {
                    "done".to_string()
                };
                Ok(text_response(&text))
            }
        })
        .build()
        .into_handle();
    let backend = sqlite_memory_store_backend().await;
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .tools(lash::tool_provider_key!("tools"), Arc::new(SearchTool))
        .build(lash_facade_tests::runtime_lease_owner())?;
    let session_id = SessionId::fixture("settle-verdict");
    let session = created_session(&core, session_id.clone())
        .await
        .open()
        .await?;
    session
        .send(TurnInput::text("search the index"))
        .output()
        .await?;
    drop(session);
    core.shutdown().await?;

    let reader = explicit_ephemeral_facets(rlm_core_builder_over(backend))
        .build(lash_facade_tests::runtime_lease_owner())?;
    let durable = reader.session(session_id).durable().await?;
    let transcript = durable.transcript().await?;
    let (turn, cell) = transcript
        .visible()
        .find_map(|entry| match &entry.item {
            lash::transcript::TranscriptItem::Cell(cell) => {
                Some((entry.provenance.turn_id.as_ref().expect("cell turn"), cell))
            }
            _ => None,
        })
        .expect("the search cell is committed");
    let [call] = cell.calls.as_slice() else {
        panic!("one executed call: {:?}", cell.calls);
    };
    let call_id = call.call_id.as_ref().expect("host call id");
    let lash::ToolCallRead::Settled { record } = durable
        .read_tool_call(&lash::TurnToolCallRef {
            turn_id: turn.clone(),
            call_id: call_id.clone(),
        })
        .await?
    else {
        panic!("the committed call has a settled body");
    };
    assert_eq!(&record.call_id, call_id);
    assert_eq!(record.tool, SEARCH_OPERATION);
    let report_tool_calls = durable.read_turn_tool_calls(turn).await?;
    assert_eq!(
        report_tool_calls.as_slice(),
        std::slice::from_ref(record.as_ref())
    );
    let lash_core::ToolCallOutcome::Failure(failure) = &record.output.outcome else {
        panic!("lash refused the host's Success: {:?}", record.output);
    };
    assert_eq!(
        serde_json::to_value(&call.outcome)?,
        serde_json::json!({
            "kind": "failed",
            "code": failure.code,
            "message": failure.message,
        }),
        "FIG-6357: the cell carries the settle refusal without a second read"
    );
    assert_eq!(failure.code, "tool_outcome_not_declared");
    assert!(
        failure.message.contains("tool `tools.search`"),
        "{failure:?}"
    );
    assert!(failure.message.contains("/diagnostics"), "{failure:?}");
    assert!(failure.message.contains("took"), "{failure:?}");
    let Some(lash_core::ToolFailureCause::Declaration {
        refusal:
            lash_vocabulary::tool_declaration::DeclarationRefusal::OutputValueMismatch {
                mismatch, ..
            },
    }) = failure.cause.as_deref()
    else {
        panic!("the committed failure carries the typed output mismatch: {failure:?}");
    };
    assert_eq!(mismatch.instance_path, "/diagnostics", "{mismatch:?}");
    assert!(mismatch.message.contains("took"), "{mismatch:?}");
    reader.shutdown().await?;
    Ok(())
}
