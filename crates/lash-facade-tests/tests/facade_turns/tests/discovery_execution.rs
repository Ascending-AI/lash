//! A tool the catalog hides from the model executes through discovery on the
//! code and batch surfaces and never on the native one, on a turn the
//! core's node runs over SQLite memory stores (FIG-5307).

use super::*;

use lash::TurnInput;
use lash_core::llm::types::{LlmOutputPart, LlmResponse};
use std::collections::VecDeque;
use tokio::sync::Mutex as TokioMutex;

struct DiscoveryTools {
    calls: Arc<StdMutex<Vec<String>>>,
}

fn definition(name: &str) -> lash_vocabulary::tool_contract::ToolDefinition {
    let mut tool = lash_vocabulary::tool_contract::ToolDefinition::raw(
        format!("tool:{name}"),
        format!("tools.{name}"),
        format!("Read {name}."),
        serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        serde_json::json!({"type":"string"}),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120));
    tool.manifest.inline = name != "hidden";
    tool
}

#[async_trait]
impl ToolProvider for DiscoveryTools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash::plugins::PluginError,
    > {
        Ok({
            ["search", "hidden"]
                .map(|name| definition(name).manifest())
                .to_vec()
        })
    }
    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        let Some(("tools", name)) = name.split_once('.') else {
            return None;
        };
        ["search", "hidden"]
            .contains(&name)
            .then(|| Arc::new(definition(name).contract()))
    }
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            self.calls.lock_recover().push(call.name().to_string());
            lash_core::ToolOutcome::ok(serde_json::json!("hidden-result"))
        })
        .await
        .into()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_hidden_tool_executes_through_rlm_and_standard_batch_but_not_native() -> Result<()>
{
    for mode in ["rlm", "batch", "native"] {
        let backend = sqlite_memory_store_backend().await;
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let first = if mode == "rlm" {
            lash_facade_tests::control_cell_response(
                0,
                ("await control.finish(await tools.hidden({}));").to_string(),
            )
        } else {
            LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "discovery-call".into(),
                    tool_name: if mode == "batch" {
                        "batch"
                    } else {
                        "tools__hidden"
                    }
                    .into(),
                    input_json: if mode == "batch" {
                        serde_json::json!({"tool_calls":[{"tool":"tools__hidden","parameters":{}}]})
                            .to_string()
                    } else {
                        "{}".into()
                    },
                    replay: None,
                }],
                ..Default::default()
            }
        };
        let responses = Arc::new(TokioMutex::new(VecDeque::from([
            first,
            text_response("done"),
        ])));
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let captured = requests.clone();
        let provider = lash::testing::TestProvider::builder()
            .kind("discovery-execution")
            .complete(move |request| {
                let responses = responses.clone();
                let captured = captured.clone();
                async move {
                    assert!(
                        request
                            .tools
                            .iter()
                            .all(|tool| tool.name != "tools__hidden")
                    );
                    captured.lock_recover().push(request.messages);
                    Ok(responses
                        .lock()
                        .await
                        .pop_front()
                        .expect("scripted response"))
                }
            })
            .build()
            .into_handle();
        let builder = if mode == "rlm" {
            LashCore::rlm_builder(
                backend.clone(),
                lash_protocol_rlm::RlmProtocolPluginFactory::new(
                    lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                        .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(
                            1_000_000,
                        ))
                        .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                        .build()
                        .with_discovery(lash_protocol_rlm::RlmDiscovery {
                            operation: "tools.search".into(),
                            catalogue_bytes: usize::MAX,
                        }),
                    lash_protocol_rlm::CellDialect::typescript(),
                ),
            )
        } else {
            LashCore::standard_builder(backend).protocol_plugin(Arc::new(
                lash_protocol_standard::StandardProtocolPluginFactory::with_config(
                    lash_protocol_standard::StandardProtocolConfig {
                        discovery: Some(lash_vocabulary::tool_contract::ToolDiscovery {
                            operation: "tools.search".into(),
                        }),
                        ..lash_protocol_standard::StandardProtocolConfig::default()
                    },
                ),
            ))
        };
        let core = explicit_ephemeral_facets(builder)
            .serve_test_llm_profile(provider, mock_llm_profile_spec())
            .tools(
                lash::tool_provider_key!("tools"),
                Arc::new(DiscoveryTools {
                    calls: calls.clone(),
                }),
            )
            .build(lash_facade_tests::runtime_lease_owner())?;
        let session = created_session(
            &core,
            lash_vocabulary::identity::SessionId::fixture(format!("discovery-{mode}")),
        )
        .await
        .open()
        .await?;
        let output = session
            .send(TurnInput::text("read the hidden tool"))
            .output()
            .await?;
        if mode == "native" {
            assert!(
                calls.lock_recover().is_empty(),
                "hidden native tool dispatched"
            );
            let output_tool_calls =
                lash::testing::tool_calls_for_activity(&session.durable(), &output.activities)
                    .await;
            let [call] = output_tool_calls.as_slice() else {
                panic!("one refused native call: {:?}", output_tool_calls);
            };
            let lash_core::ToolCallOutcome::Failure(failure) = &call.output.outcome else {
                panic!("the unlisted tool is refused: {call:?}");
            };
            assert_eq!(call.tool, "tools.hidden");
            assert_eq!(failure.code, "unknown_tool");
            assert_eq!(failure.class, lash_core::ToolFailureClass::Unavailable);
            assert_eq!(
                failure.source,
                lash_protocol_types::ToolFailureSource::Runtime
            );
            let requests = requests.lock_recover();
            assert_eq!(requests.len(), 2, "the model receives the refusal once");
            let results = requests[1]
                .iter()
                .flat_map(|message| message.blocks.iter())
                .filter_map(|block| match block {
                    lash_core::llm::types::LlmContentBlock::ToolResult {
                        call_id,
                        tool_name,
                        status,
                        ..
                    } => Some((call_id.as_str(), tool_name.as_deref(), *status)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                results,
                vec![(
                    "discovery-call",
                    Some("tools__hidden"),
                    lash_protocol_types::ToolCallStatus::Failure
                )]
            );
        } else {
            assert_eq!(*calls.lock_recover(), ["tools.hidden"], "{mode}");
            if mode == "rlm" {
                assert!(output.is_success(), "{mode}");
            } else {
                let encoded = serde_json::to_string(&output.activities)?;
                assert!(encoded.contains("hidden-result"), "{encoded}");
            }
        }
    }
    Ok(())
}
