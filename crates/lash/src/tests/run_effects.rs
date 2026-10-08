//! What a run's tool calls leave behind on facade turns: a tool check that
//! aborts the run commits its stop and settles the input, and an attachment
//! a tool puts during the turn is held by the session's commit, not by an
//! upload.

use super::*;
use crate::support::TurnOutcome;

fn echo_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:echo_tool",
        "echo_tool",
        "Echo the value.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
}

struct EchoTool;

#[async_trait]
impl ToolProvider for EchoTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![echo_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "echo_tool").then(|| Arc::new(echo_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(call.args.clone()).into()
    }
}

/// A provider that calls `tool` once, then answers "done".
fn one_tool_call(tool: &'static str) -> ProviderHandle {
    let calls = Arc::new(AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("run-effects")
        .complete(move |_| {
            let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
            async move {
                Ok(if first {
                    LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: format!("{tool}-call"),
                            tool_name: tool.to_string(),
                            input_json: r#"{"value":"x"}"#.to_string(),
                            replay: None,
                        }],
                        ..LlmResponse::default()
                    }
                } else {
                    text_response("done")
                })
            }
        })
        .build()
        .into_handle()
}

/// A tool check's `AbortRun` stops the run: the run commits with the
/// plugin-abort stop and settles its input.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admitted_plugin_abort_commits_and_settles_input() -> Result<()> {
    let abort: Arc<dyn lash_core::facade_support::PluginFactory> =
        Arc::new(StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("tool-policy"),
            lash_core::facade_support::PluginSpec::new().with_tool_args_check(
                lash_core::hook_key!("policy"),
                Arc::new(|_| {
                    Box::pin(async {
                        Ok(lash_core::plugin::BeforeToolDecision::AbortRun(
                            lash_core::plugin::PluginAbort::new(
                                "blocked",
                                "plugin stopped admitted turn",
                            ),
                        ))
                    })
                }),
            ),
        ));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(one_tool_call("echo_tool"), mock_llm_profile_spec())
    .tools(Arc::new(EchoTool))
    .plugin(abort)
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("admitted-plugin-abort").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    let handle = session.send(TurnInput::text("abort this input")).await?;
    let input = handle.input_id().clone();
    let report = handle.output().await?.result;

    assert!(
        matches!(
            report.outcome,
            TurnOutcome::Stopped(lash_core::facade_support::TurnStop::PluginAbort)
        ),
        "{:?}",
        report.outcome
    );
    assert!(
        session
            .durable()
            .pending_turn_inputs()
            .await?
            .iter()
            .all(|pending| pending.input.input_id != input),
        "the aborted run settles its input"
    );
    Ok(())
}

fn attachment_put_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:attachment_put",
        "attachment_put",
        "Write an attachment through the active runtime facade.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
}

/// A tool that puts one attachment and answers it.
struct AttachmentPutTool;

#[async_trait]
impl ToolProvider for AttachmentPutTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![attachment_put_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "attachment_put").then(|| Arc::new(attachment_put_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let reference = match call
            .context
            .attachments()
            .put(
                b"turn-owned-tool-attachment".to_vec(),
                lash_core::AttachmentCreateMeta::new(
                    lash_core::MediaType::parse("image/png").expect("media type"),
                    Some(lash_core::AttachmentTypeMetadata::image(Some(1), Some(1))),
                    Some("turn-owned.png".to_string()),
                ),
            )
            .await
        {
            Ok(reference) => reference,
            Err(error) => return lash_core::ToolOutcome::err_fmt(error).into(),
        };
        lash_core::ToolOutcome::from_output(lash_core::ToolCallOutput::success_tool_value(
            lash_core::ToolValue::Attachment(lash_core::AttachmentSource::stored(reference)),
        ))
        .into()
    }
}

/// An attachment a tool puts during a turn is held by the turn's
/// execution and then by the session's commit: never by an upload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "FIG-5357: a durable turn's tool put is staged on an expiring upload, not held by the turn's execution"]
async fn stream_turn_tool_put_is_bound_to_the_turn_id() -> Result<()> {
    let backend = sqlite_memory_store_backend().await;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(one_tool_call("attachment_put"), mock_llm_profile_spec())
        .tools(Arc::new(AttachmentPutTool))
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("attachment-owner").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    let output = session
        .send(TurnInput::text("store an attachment"))
        .output()
        .await?;
    assert_eq!(output.assistant_message(), Some("done"));

    let id = lash_core::attachments::content_id(b"turn-owned-tool-attachment");
    let referrers = backend
        .session_store_factory()
        .attachment_referrers(&id)
        .await
        .expect("read the attachment's referrers");
    assert!(
        referrers.contains(&lash_core::ArtifactReferrer::Session(
            lash_core::SessionId::from("attachment-owner")
        )),
        "the commit acquired the session's edge: {referrers:?}"
    );
    assert!(
        referrers
            .iter()
            .all(|referrer| !matches!(referrer, lash_core::ArtifactReferrer::Upload(_))),
        "a turn's put is held by its execution, not an upload: {referrers:?}"
    );
    Ok(())
}
