use super::*;

struct DiscoveryTools {
    calls: Arc<StdMutex<Vec<String>>>,
}

fn definition(name: &str) -> lash_core::ToolDefinition {
    let mut tool = lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        format!("Read {name}."),
        serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        serde_json::json!({"type":"string"}),
    )
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["tools"], name));
    tool.manifest.inline = name != "hidden";
    tool
}

#[async_trait]
impl ToolProvider for DiscoveryTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        ["search", "hidden"]
            .map(|name| definition(name).manifest())
            .to_vec()
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        ["search", "hidden"]
            .contains(&name)
            .then(|| Arc::new(definition(name).contract()))
    }
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        self.calls.lock_recover().push(call.name.to_string());
        lash_core::ToolOutcome::ok(serde_json::json!("hidden-result"))
    }
}

#[tokio::test]
async fn discovery_hidden_tool_executes_through_rlm_and_standard_batch_but_not_native() -> Result<()>
{
    for mode in ["rlm", "batch", "native"] {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let first = if mode == "rlm" {
            text_response(&lashlang_block("finish await tools.hidden({})?"))
        } else {
            LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "discovery-call".into(),
                    tool_name: if mode == "batch" { "batch" } else { "hidden" }.into(),
                    input_json: if mode == "batch" {
                        serde_json::json!({"tool_calls":[{"tool":"hidden","parameters":{}}]})
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
        let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
        let captured = requests.clone();
        let provider = crate::testing::TestProvider::builder()
            .kind("discovery-execution")
            .complete(move |request| {
                let responses = responses.clone();
                let captured = captured.clone();
                async move {
                    assert!(request.tools.iter().all(|tool| tool.name != "hidden"));
                    captured
                        .lock_recover()
                        .push(serde_json::to_string(&request.messages).unwrap());
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
                crate::TurnBudget::Unbounded,
                lash_protocol_rlm::RlmProtocolPluginFactory::new(
                    lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                        .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(
                            1_000_000,
                        ))
                        .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
                        .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                        .build()
                        .with_discovery(lash_core::ToolDiscovery {
                            operation: "tools.search".into(),
                        }),
                    inmem_artifact_store(),
                ),
            )
        } else {
            LashCore::standard_builder(crate::TurnBudget::Unbounded).protocol_plugin(Arc::new(
                lash_protocol_standard::StandardProtocolPluginFactory::with_config(
                    lash_protocol_standard::StandardProtocolConfig {
                        discovery: Some(lash_core::ToolDiscovery {
                            operation: "search".into(),
                        }),
                    },
                ),
            ))
        };
        let core = explicit_ephemeral_facets(builder)
            .provider(provider)
            .model(mock_model_spec())
            .tools(Arc::new(DiscoveryTools {
                calls: calls.clone(),
            }))
            .store_factory(Arc::new(
                lash_core::facade_support::InMemorySessionStoreFactory::new(),
            ))
            .process_registry(Arc::new(TestLocalProcessRegistry::default()))
            .build(crate::testing::runtime_lease_owner())?;
        let session = core.session(format!("discovery-{mode}")).open().await?;
        let output = session
            .turn(TurnInput::text("read the hidden tool"))
            .run()
            .await?;
        if mode == "native" {
            assert!(
                calls.lock_recover().is_empty(),
                "hidden native tool dispatched"
            );
            let encoded = requests.lock_recover().join("\n");
            assert!(
                encoded.contains("unknown_tool"),
                "typed unknown-tool refusal: {encoded}"
            );
        } else {
            assert_eq!(*calls.lock_recover(), ["hidden"], "{mode}");
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
