//! Only a failed cell's next prompt names its completed host calls with
//! their durable ids and bounded results (FIG-6293, FIG-6338).
//!
//! Real RLM turns on the core's own node over a SQLite memory store set, in
//! both dialects. The model's next request is read as
//! the scripted provider received it.

use super::*;
use lash_core::llm::types::LlmResponse;
use lash_facade_tests::control_cell_response;

type Requests = Arc<StdMutex<Vec<serde_json::Value>>>;
type Responses = Arc<tokio::sync::Mutex<std::collections::VecDeque<LlmResponse>>>;

fn message_strings<'a>(value: &'a serde_json::Value, found: &mut Vec<&'a str>) {
    match value {
        serde_json::Value::String(text) => found.push(text),
        serde_json::Value::Array(items) => {
            items.iter().for_each(|item| message_strings(item, found))
        }
        serde_json::Value::Object(map) => {
            map.values().for_each(|item| message_strings(item, found))
        }
        _ => {}
    }
}

#[derive(Default)]
struct ReceiptTools {
    pages: StdMutex<Vec<serde_json::Value>>,
}

fn receipt_definition(name: &str) -> lash_vocabulary::tool_contract::ToolDefinition {
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        format!("tool:{name}"),
        format!("tools.{name}"),
        "Write a fixture page.",
        lash_vocabulary::tool_contract::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object"}),
    )
    .expect("receipt tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

#[async_trait]
impl ToolProvider for ReceiptTools {
    fn tool_manifests(
        &self,
        config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash::plugins::PluginError,
    > {
        let mut manifests = AppTools.tool_manifests(config)?;
        manifests
            .extend(["create_page", "reject_page"].map(|name| receipt_definition(name).manifest()));
        Ok(manifests)
    }

    fn resolve_contract(
        &self,
        name: &str,
        config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        match name {
            "tools.create_page" => Some(Arc::new(receipt_definition("create_page").contract())),
            "tools.reject_page" => Some(Arc::new(receipt_definition("reject_page").contract())),
            _ => AppTools.resolve_contract(name, config),
        }
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        match call.name().as_str() {
            "tools.create_page" => {
                self.pages.lock_recover().push(call.args.clone());
                lash_core::ToolOutcome::ok(call.args.clone()).into()
            }
            "tools.reject_page" => lash_core::ToolOutcome::err_fmt("page write rejected").into(),
            _ => AppTools.execute(call).await,
        }
    }
}

/// A core whose sessions are created under `presentation`, answering from
/// `responses` and recording each request's messages in `requests`.
fn core(
    backend: lash_execution_contracts::backend::Backend,
    dialect: lash::rlm::CellDialect,

    presentation: lash::rlm::RlmPresentationConfig,
    responses: &Responses,
    requests: &Requests,
) -> Result<LashCore> {
    core_with_feedback(backend, dialect, presentation, responses, requests, None)
}

fn core_with_feedback(
    backend: lash_execution_contracts::backend::Backend,
    dialect: lash::rlm::CellDialect,

    presentation: lash::rlm::RlmPresentationConfig,
    responses: &Responses,
    requests: &Requests,
    feedback: Option<lash::plugins::CellFailureFeedbackHook>,
) -> Result<LashCore> {
    let (responses, requests) = (Arc::clone(responses), Arc::clone(requests));
    let provider = lash::testing::TestProvider::builder()
        .kind("cell-call-feedback")
        .complete(move |request: LlmRequest| {
            let responses = Arc::clone(&responses);
            requests
                .lock_recover()
                .push(serde_json::to_value(&request.messages).expect("serialize request messages"));
            async move {
                Ok(responses
                    .lock()
                    .await
                    .pop_front()
                    .expect("a scripted response"))
            }
        })
        .build()
        .into_handle();
    let mut config = lash::rlm::RlmProtocolPluginConfig::builder()
        .instruction_limit(lash::rlm::InstructionBound::standard())
        .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
        .build();
    config.presentation = presentation;
    let factory = lash::rlm::RlmProtocolPluginFactory::new(config, dialect)
        .with_worker_service(untimed_fixture_workers());
    let builder = explicit_ephemeral_facets(LashCore::rlm_builder(backend, factory))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .plugin(Arc::new(lash::plugins::StaticPluginFactory::new(
            lash::plugins::PluginDeclaration::initial("tools"),
            lash_core::testing::tool_spec(
                "tools",
                lash::tool_provider_key!("tools"),
                Arc::new(ReceiptTools::default()),
            ),
        )));
    let builder = if let Some(feedback) = feedback {
        builder.plugin(Arc::new(lash::plugins::StaticPluginFactory::new(
            lash::plugins::PluginDeclaration::initial("cell-repairs"),
            lash::plugins::PluginSpec::new()
                .with_cell_failure_feedback(lash::hook_key!("callable"), feedback),
        )))
    } else {
        builder
    };
    builder.build(lash_facade_tests::runtime_lease_owner())
}

/// The text in `messages` that carries a calls block, wherever the channel
/// placed the observation.
fn calls_observation(messages: &serde_json::Value) -> String {
    let mut found = Vec::new();
    message_strings(messages, &mut found);
    let observations = found
        .into_iter()
        .filter(|text| text.contains("{\"completed_calls\":"))
        .collect::<Vec<_>>();
    assert_eq!(observations.len(), 1, "one cell lists its calls");
    observations[0].to_string()
}

/// FIG-6338: success keeps the concise operation/outcome feedback from
/// before FIG-6293, without exposing durable ids or result summaries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_cell_feedback_omits_completed_call_evidence() -> Result<()> {
    for dialect in [
        lash::rlm::CellDialect::typescript(),
        lash::rlm::CellDialect::python(),
    ] {
        {
            let case = format!("{} native", dialect.name());
            let code = if dialect.name() == "python" {
                "found = await tools.app_lookup({})"
            } else {
                "const found = await tools.appLookup({});"
            };
            let responses: Responses = Arc::new(tokio::sync::Mutex::new(
                [
                    control_cell_response(0, code.to_string()),
                    text_response("The lookup succeeded."),
                ]
                .into(),
            ));
            let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
            let running = core(
                sqlite_memory_store_backend().await,
                dialect.clone(),
                lash::rlm::RlmPresentationConfig::standard(),
                &responses,
                &requests,
            )?;
            let session = created_session(
                &running,
                SessionId::fixture("successful-cell-call-feedback"),
            )
            .await
            .open()
            .await?;
            let output = session
                .send(TurnInput::text("look up once"))
                .output()
                .await?;
            let output_tool_calls =
                lash::testing::tool_calls_for_activity(&session.durable(), &output.activities)
                    .await;
            let lookup = output_tool_calls
                .iter()
                .find(|record| record.tool == "tools.app_lookup")
                .unwrap_or_else(|| panic!("{case}: the lookup completed"));
            {
                let requests = requests.lock_recover();
                let mut texts = Vec::new();
                message_strings(&requests[1], &mut texts);
                assert!(
                    texts.iter().all(|text| !text.contains("completed_calls")),
                    "{case}: successful feedback has no completed_calls block: {texts:?}"
                );
                assert!(
                    texts
                        .iter()
                        .all(|text| !text.contains(lookup.call_id.as_str())),
                    "{case}: the durable call id stays out of successful feedback"
                );
                assert!(
                    texts
                        .iter()
                        .any(|text| text.contains("Calls:\n- tools.app_lookup → ok")),
                    "{case}: concise call feedback is preserved: {texts:?}"
                );
            }
            drop(session);
            running.shutdown().await?;
        }
    }
    Ok(())
}

/// FIG-7530: a failed cell starts with its error, then names a completed
/// write by its whole receipt beyond the old 512-byte prefix. A later failed
/// call stays separate. Reopening renders the same recorded observation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_cell_feedback_preserves_completed_call_evidence() -> Result<()> {
    for dialect in [
        lash::rlm::CellDialect::typescript(),
        lash::rlm::CellDialect::python(),
    ] {
        {
            let case = format!("{} native", dialect.name());
            let path = format!("wiki/{}", "receipt-".repeat(12));
            let receipt = serde_json::json!({"body": "poem ".repeat(110), "path": path});
            let input = serde_json::to_string(&receipt)?;
            assert!(input.find(&path).expect("fixture path") > 512);
            let failure = "x1 failure after committed write";
            let code = if dialect.name() == "python" {
                format!(
                    "found = await tools.create_page({input})\ntry:\n    await tools.reject_page({{}})\nexcept Exception:\n    pass\nprint('printed after calls')\nraise ValueError('{failure}')"
                )
            } else {
                format!(
                    "const found = await tools.createPage({input});\ntry {{ await tools.rejectPage({{}}); }} catch (error) {{}}\nconsole.log('printed after calls');\nthrow new Error(\"{failure}\");"
                )
            };
            let responses: Responses = Arc::new(tokio::sync::Mutex::new(
                [
                    control_cell_response(0, code.to_string()),
                    text_response("The lookup ran; the cell failed after it."),
                ]
                .into(),
            ));
            let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
            let backend = sqlite_memory_store_backend().await;
            let session_id = SessionId::fixture("cell-call-feedback");
            let running = core(
                backend.clone(),
                dialect.clone(),
                lash::rlm::RlmPresentationConfig::standard(),
                &responses,
                &requests,
            )?;
            let session = created_session(&running, session_id.clone())
                .await
                .open()
                .await?;
            let output = session
                .send(TurnInput::text("look up, then fail"))
                .output()
                .await?;
            let output_tool_calls =
                lash::testing::tool_calls_for_activity(&session.durable(), &output.activities)
                    .await;
            let lookup = output_tool_calls
                .iter()
                .find(|record| record.tool == "tools.create_page")
                .unwrap_or_else(|| panic!("{case}: the turn's ledger holds the write"));
            let transcript = committed(&session)
                .await
                .transcript()
                .expect("valid committed history");
            let cell = transcript
                .visible()
                .find_map(|entry| match &entry.item {
                    lash::transcript::TranscriptItem::Cell(cell) => Some(cell),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{case}: the failed cell is committed"));
            assert!(cell.result.is_failed(), "{case}: {:?}", cell.result);
            let [call, rejected] = cell.calls.as_slice() else {
                panic!("{case}: a completed and a failed call: {:?}", cell.calls);
            };
            assert_eq!(call.call_id.as_ref(), Some(&lookup.call_id), "{case}");
            assert_eq!(lookup.args, receipt, "the write used the fixture receipt");
            let failed_record = output_tool_calls
                .iter()
                .find(|record| record.tool == "tools.reject_page")
                .expect("the rejected write is recorded");
            assert_eq!(rejected.call_id.as_ref(), Some(&failed_record.call_id));
            assert!(!failed_record.output.is_success());

            let observation = calls_observation(&requests.lock_recover()[1]);
            let lash_core::CellOutcome::Failed(recorded_failure) = &cell.result else {
                panic!("fixture cell must fail");
            };
            assert!(recorded_failure.message.contains(failure));
            let error_first = observation.starts_with(&recorded_failure.message);
            let whole_identifier = observation.contains(&path);
            assert!(
                error_first && whole_identifier,
                "{case}: error first={error_first}, whole identifier={whole_identifier}: {observation}"
            );
            let block = observation
                .split("Calls:\n")
                .nth(1)
                .expect("calls")
                .split("\n\n")
                .next()
                .expect("calls block");
            let block: serde_json::Value = serde_json::from_str(block)?;
            assert_eq!(
                block["completed_calls"]
                    .as_array()
                    .expect("completed calls")
                    .len(),
                1
            );
            assert_eq!(block["completed_calls"][0]["tool"], "tools.create_page");
            assert_eq!(
                block["completed_calls"][0]["call_id"],
                serde_json::json!(lookup.call_id)
            );
            assert_eq!(
                block["failed_calls"]
                    .as_array()
                    .expect("failed calls")
                    .len(),
                1
            );
            assert_eq!(
                block["failed_calls"][0]["call_id"],
                serde_json::json!(failed_record.call_id)
            );
            assert_eq!(block["failed_calls"][0]["outcome"], "err");
            assert!(
                observation.find("Calls:").expect("calls")
                    < observation.find("Output:").expect("prints")
            );
            assert!(
                observation.contains(
                    "The completed calls above remain completed even though this cell failed."
                ),
                "{case}: {observation}"
            );
            drop(session);
            running.shutdown().await?;

            // Another core, whose live cuts would omit every call.
            let mut live = lash::rlm::RlmPresentationConfig::standard();
            live.prompt_max_tool_calls = 0;
            live.call_feedback = lash::rlm::CallFeedbackPolicy::new(
                0,
                lash::rlm::CallFeedbackPolicy::MIN_BLOCK_BYTES,
            )
            .expect("minimum block is valid");
            responses
                .lock()
                .await
                .push_back(text_response("Nothing more to do."));
            let reopened = core(backend, dialect.clone(), live, &responses, &requests)?;
            reopened
                .session(session_id)
                .open()
                .await?
                .send(TurnInput::text("anything else?"))
                .output()
                .await?;
            assert_eq!(
                calls_observation(&requests.lock_recover()[2]),
                observation,
                "{case}: the reopened session renders the recorded observation"
            );
            reopened.shutdown().await?;
        }
    }
    Ok(())
}

/// FIG-6342 / host-owned repair policy: only an installed plugin adds hints,
/// and its typed diagnosis reaches the model's very next input in both dialects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugins_own_cell_failure_repairs_in_the_next_model_input() -> Result<()> {
    for (dialect, code, diagnostic_code, callable) in [
        (
            lash::rlm::CellDialect::typescript(),
            "await app_lookup({});",
            "TS_UNKNOWN_BINDING",
            "tools.appLookup",
        ),
        (
            lash::rlm::CellDialect::python(),
            "await app_lookup({})",
            "PY_UNKNOWN_NAME",
            "tools.app_lookup",
        ),
    ] {
        let mut baseline = None;
        for installed in [false, true] {
            let hook: lash::plugins::CellFailureFeedbackHook = Arc::new(move |diagnostic| {
                assert_eq!(diagnostic.code, diagnostic_code);
                assert_eq!(diagnostic.unknown_name, Some("app_lookup"));
                let (start, end) = diagnostic.span.expect("unknown name has a span");
                assert_eq!(&code[start..end], "app_lookup");
                assert_eq!(
                    diagnostic
                        .grants
                        .get("tools.app_lookup")
                        .map(String::as_str),
                    Some(callable)
                );
                vec![format!("did you mean `{callable}`?")]
            });
            let responses: Responses = Arc::new(tokio::sync::Mutex::new(
                [
                    control_cell_response(0, code.to_owned()),
                    text_response("done"),
                ]
                .into(),
            ));
            let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
            let running = core_with_feedback(
                sqlite_memory_store_backend().await,
                dialect.clone(),
                lash::rlm::RlmPresentationConfig::standard(),
                &responses,
                &requests,
                installed.then_some(hook),
            )?;
            let session = created_session(&running, SessionId::fixture("plugin-cell-repairs"))
                .await
                .open()
                .await?;
            session
                .send(TurnInput::text("call the lookup"))
                .output()
                .await?;
            let feedback = {
                let requests = requests.lock_recover();
                assert_eq!(requests.len(), 2);
                let mut texts = Vec::new();
                message_strings(&requests[1], &mut texts);
                texts
                    .into_iter()
                    .find(|text| text.contains(diagnostic_code))
                    .expect("the next model request carries the diagnosis")
                    .to_owned()
            };
            let repair = format!("\nInstead: did you mean `{callable}`?");
            if installed {
                assert!(feedback.contains(&repair), "{feedback}");
                assert_eq!(
                    feedback.replace(&repair, ""),
                    baseline.as_deref().expect("unextended feedback"),
                    "the plugin adds only its repair"
                );
            } else {
                let message = if dialect.name() == "python" {
                    "name `app_lookup` is not defined"
                } else {
                    "`app_lookup` is not defined"
                };
                assert_eq!(
                    feedback,
                    format!(
                        "{diagnostic_code}: {message} (line 1, column 7)\n  {code}\n\nNext: the defect is in the program, not in what the runtime allows. Fix the cause named above, then send the corrected cell.",
                    ),
                    "no plugin preserves the original feedback bytes"
                );
                baseline = Some(feedback);
            }
            running.shutdown().await?;
        }
    }
    Ok(())
}

/// FIG-7295: execution errors receive the same parent-owned repair contribution
/// as lowering refusals, and the very next model input contains its answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_cell_failure_repairs_reach_the_next_model_input() -> Result<()> {
    for (dialect, code, callable) in [
        (
            lash::rlm::CellDialect::typescript(),
            "throw 17;",
            "tools.appLookup",
        ),
        (
            lash::rlm::CellDialect::python(),
            "raise ValueError('stop')",
            "tools.app_lookup",
        ),
    ] {
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook: lash::plugins::CellFailureFeedbackHook = {
            let seen = Arc::clone(&seen);
            Arc::new(move |diagnostic| {
                assert_eq!(diagnostic.unknown_name, None);
                assert_eq!(diagnostic.span, None);
                assert_eq!(
                    diagnostic
                        .grants
                        .get("tools.app_lookup")
                        .map(String::as_str),
                    Some(callable)
                );
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                vec!["handle the failed computation".into()]
            })
        };
        let responses: Responses = Arc::new(tokio::sync::Mutex::new(
            [
                control_cell_response(0, code.to_owned()),
                text_response("done"),
            ]
            .into(),
        ));
        let requests: Requests = Arc::new(StdMutex::new(Vec::new()));
        let running = core_with_feedback(
            sqlite_memory_store_backend().await,
            dialect,
            lash::rlm::RlmPresentationConfig::standard(),
            &responses,
            &requests,
            Some(hook),
        )?;
        let session = created_session(&running, SessionId::fixture("runtime-plugin-cell-repairs"))
            .await
            .open()
            .await?;
        session
            .send(TurnInput::text("run the computation"))
            .output()
            .await?;
        {
            let requests = requests.lock_recover();
            assert_eq!(requests.len(), 2);
            let mut texts = Vec::new();
            message_strings(&requests[1], &mut texts);
            assert!(
                texts
                    .iter()
                    .any(|text| text.contains("\nInstead: handle the failed computation")),
                "{texts:?}"
            );
        }
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);
        running.shutdown().await?;
    }
    Ok(())
}
