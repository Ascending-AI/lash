//! A presentation step changes what the model observes of a tool result and
//! nothing the session records, through a host's `send()` on the core's node
//! over SQLite memory stores (FIG-5307; the runtime law FIG-5190 deleted with
//! the engine double).

use super::*;

use lash::TurnInput;
use lash_core::llm::types::LlmOutputPart;
use lash_core::testing::runtime_helpers::EchoTool;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn presentation_step_only_changes_model_observation() -> Result<()> {
    let seen = Arc::new(StdMutex::new(Vec::<String>::new()));
    let replies = Arc::new(StdMutex::new(std::collections::VecDeque::from([
        LlmResponse {
            parts: vec![
                LlmOutputPart::Text {
                    text: "checking tool".to_owned(),
                    response_meta: None,
                },
                LlmOutputPart::ToolCall {
                    call_id: "tool-1".to_owned(),
                    tool_name: "tools__echo_tool".to_owned(),
                    input_json: r#"{"value":"sample"}"#.to_owned(),
                    replay: None,
                },
            ],
            ..LlmResponse::default()
        },
        text_response("done"),
    ])));
    let seen_by_model = Arc::clone(&seen);
    let provider = lash::testing::TestProvider::builder()
        .kind("mock")
        .complete(move |request| {
            seen_by_model.lock_recover().extend(
                request
                    .messages
                    .iter()
                    .flat_map(|message| message.blocks.iter())
                    .filter(|block| matches!(block, LlmContentBlock::ToolResult { .. }))
                    .map(|block| format!("{block:?}")),
            );
            let reply = replies.lock_recover().pop_front().expect("scripted reply");
            async move { Ok(reply) }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .tools(lash::tool_provider_key!("tools"), Arc::new(EchoTool))
    .plugin(Arc::new(StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("model-projection"),
        lash_core::facade_support::PluginSpec::new().with_presentation_step(
            lash::hook_key!("model-projection"),
            Arc::new(|input: lash_core::facade_support::ToolPresentationInput| {
                let projected = lash_protocol_types::ModelToolReturn {
                    outcome: lash_protocol_types::ModelToolOutcome::Completed,
                    tool_name: input.context.tool_name.clone(),
                    parts: vec![lash_protocol_types::ModelToolReturnPart::text(
                        "model projection",
                    )],
                };
                Box::pin(async move { Ok(projected) })
            }),
        ),
    )))
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session = created_session(
        &core,
        lash::SessionId::parse("projection").expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;

    let turn = session
        .send(TurnInput::text("run the tool"))
        .output()
        .await?;

    let observed = seen.lock_recover().clone();
    assert!(
        observed
            .iter()
            .any(|block| block.contains("model projection") && !block.contains("raw:sample")),
        "the model observes the presented result: {observed:?}"
    );
    let turn_tool_calls =
        lash::testing::tool_calls_for_activity(&session.durable(), &turn.activities).await;
    let calls = &turn_tool_calls;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].provider_call_id.as_deref(), Some("tool-1"));
    assert_eq!(
        calls[0].output.value_for_projection(),
        serde_json::json!({ "payload": "raw:sample" }),
        "the session records the tool's own result"
    );
    core.shutdown().await?;
    Ok(())
}
