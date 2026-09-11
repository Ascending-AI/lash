use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[test]
fn standard_discovery_filters_provider_specs_and_requires_an_inline_member() {
    let tool = |name: &str, inline| {
        let mut tool = lash_core::ToolDefinition::raw(
            name,
            name,
            name,
            serde_json::json!({"type":"object"}),
            serde_json::json!({"type":"string"}),
        );
        tool.manifest.inline = inline;
        tool
    };
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![
        tool("tools.search", true),
        tool("hidden", false),
    ]);
    let manifests = catalog
        .tools
        .iter()
        .map(|entry| entry.manifest.clone())
        .collect::<Vec<_>>();
    for discovery in [
        None,
        Some(lash_core::ToolDiscovery {
            operation: "tools.search".into(),
        }),
    ] {
        validate_discovery(&manifests, discovery.as_ref()).unwrap();
        let expected = if discovery.is_some() { 1 } else { 2 };
        let driver = StandardProtocolDriver {
            config: StandardProtocolConfig { discovery },
        };
        let preamble = driver.build_preamble(ProtocolBuildInput {
            tool_catalog: Arc::new(catalog.clone()),
            plugin_extensions: Default::default(),
            trigger_events: Default::default(),
            extra_prompt_contributions: Vec::new(),
        });
        assert_eq!(preamble.tool_specs.len(), expected);
    }
    for operation in ["hidden", "absent"] {
        assert!(matches!(
            validate_discovery(
                &manifests,
                Some(&lash_core::ToolDiscovery {
                    operation: operation.into()
                })
            ),
            Err(PluginError::InvalidToolDiscovery { .. })
        ));
    }
}

#[derive(Clone, Debug)]
struct DiscoveryRefusalProvider {
    mixed: bool,
    calls: Arc<AtomicUsize>,
    saw_refusal_text: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl lash_core::facade_support::Provider for DiscoveryRefusalProvider {
    fn kind(&self) -> &'static str {
        "stub"
    }

    fn route_identity(&self, model: &str) -> lash_core::ProviderRouteIdentity {
        lash_core::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }

    fn options(&self) -> lash_core::facade_support::ProviderOptions {
        lash_core::facade_support::ProviderOptions::default()
    }

    fn set_options(&mut self, _options: lash_core::facade_support::ProviderOptions) {}

    fn serialize_config(&self) -> Value {
        serde_json::json!({})
    }

    async fn complete(
        &mut self,
        request: lash_core::LlmRequest,
    ) -> Result<lash_core::LlmResponse, lash_core::facade_support::LlmTransportError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let mut parts = vec![lash_core::LlmOutputPart::ToolCall {
                call_id: "refused-call".to_string(),
                tool_name: "catalog_only".to_string(),
                input_json: r#"{"probe":"refused"}"#.to_string(),
                replay: None,
            }];
            if self.mixed {
                parts.push(lash_core::LlmOutputPart::ToolCall {
                    call_id: "admitted-call".to_string(),
                    tool_name: "tools.search".to_string(),
                    input_json: r#"{"probe":"admitted"}"#.to_string(),
                    replay: None,
                });
            }
            return Ok(lash_core::LlmResponse {
                parts,
                ..lash_core::LlmResponse::default()
            });
        }

        let rendered_request = format!("{:?}", request.messages);
        self.saw_refusal_text.store(
            rendered_request.contains(
                "Tool `catalog_only` was not listed in this request; use a listed discovery operation or batch.",
            ),
            Ordering::SeqCst,
        );
        Ok(lash_core::LlmResponse {
            parts: vec![lash_core::LlmOutputPart::Text {
                text: "done".to_string(),
                response_meta: None,
            }],
            ..lash_core::LlmResponse::default()
        })
    }

    fn clone_boxed(&self) -> Box<dyn lash_core::facade_support::Provider> {
        Box::new(self.clone())
    }
}

#[derive(Debug)]
struct DiscoveryRefusalTools {
    admitted_executions: Arc<AtomicUsize>,
    refused_executions: Arc<AtomicUsize>,
}

fn discovery_runtime_tool(name: &str, inline: bool) -> lash_core::ToolDefinition {
    let mut tool = lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "discovery refusal regression tool",
        serde_json::json!({
            "type": "object",
            "properties": { "probe": { "type": "string" } },
            "required": ["probe"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "string" }),
    );
    tool.manifest.inline = inline;
    tool
}

#[async_trait::async_trait]
impl ToolProvider for DiscoveryRefusalTools {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![
            discovery_runtime_tool("tools.search", true).manifest(),
            discovery_runtime_tool("catalog_only", false).manifest(),
        ]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<ToolContract>> {
        match name {
            "tools.search" => Some(Arc::new(
                discovery_runtime_tool("tools.search", true).contract(),
            )),
            "catalog_only" => Some(Arc::new(
                discovery_runtime_tool("catalog_only", false).contract(),
            )),
            _ => None,
        }
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        match call.name {
            "tools.search" => {
                self.admitted_executions.fetch_add(1, Ordering::SeqCst);
                ToolOutcome::ok(serde_json::json!("admitted"))
            }
            "catalog_only" => {
                self.refused_executions.fetch_add(1, Ordering::SeqCst);
                ToolOutcome::ok(serde_json::json!("must not run"))
            }
            name => panic!("unexpected discovery test tool: {name}"),
        }
    }
}

fn trace_lifecycle_count(entries: &[Value], call_id: &str, kind: &str) -> usize {
    entries
        .iter()
        .filter(|entry| {
            entry.get("type").and_then(Value::as_str) == Some(kind)
                && entry.get("call_id").and_then(Value::as_str) == Some(call_id)
        })
        .count()
}

async fn assert_discovery_refusal_is_reported_and_accounted(mixed: bool) {
    let calls = Arc::new(AtomicUsize::new(0));
    let saw_refusal_text = Arc::new(AtomicBool::new(false));
    let admitted_executions = Arc::new(AtomicUsize::new(0));
    let refused_executions = Arc::new(AtomicUsize::new(0));
    let provider = DiscoveryRefusalProvider {
        mixed,
        calls: Arc::clone(&calls),
        saw_refusal_text: Arc::clone(&saw_refusal_text),
    };
    let provider_handle = lash_core::facade_support::ProviderHandle::new(
        lash_core::facade_support::ProviderComponents::new(Box::new(provider)),
    );
    let trace_path = std::env::temp_dir().join(format!(
        "lash-standard-discovery-refusal-{}-{}-{}.jsonl",
        if mixed { "mixed" } else { "all" },
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let mut host = lash_core::facade_support::RuntimeHostConfig::in_memory(
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    host.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider_handle),
    );
    host.tracing.trace_sink = Some(Arc::new(lash_core::facade_support::JsonlTraceSink::new(
        trace_path.clone(),
    )));
    let tools: Arc<dyn ToolProvider> = Arc::new(DiscoveryRefusalTools {
        admitted_executions: Arc::clone(&admitted_executions),
        refused_executions: Arc::clone(&refused_executions),
    });
    let factories: Vec<Arc<dyn lash_core::facade_support::PluginFactory>> = vec![
        Arc::new(StandardProtocolPluginFactory::with_config(
            StandardProtocolConfig {
                discovery: Some(lash_core::ToolDiscovery {
                    operation: "tools.search".to_string(),
                }),
            },
        )),
        Arc::new(lash_core::plugin::StaticPluginFactory::new(
            "discovery-refusal-tools",
            lash_core::facade_support::PluginSpec::new().with_tool_provider(tools),
        )),
    ];
    let policy = lash_core::SessionPolicy {
        provider_id: "stub".to_string(),
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("valid model"),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let session_id = if mixed {
        "discovery-refusal-mixed"
    } else {
        "discovery-refusal-all"
    };
    let scoped_controller = lash_core::ScopedEffectController::shared(
        Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default()),
        lash_core::ExecutionScope::turn(session_id, "turn-1"),
    )
    .expect("scoped controller");
    let mut runtime = Box::pin(
        lash_core::facade_support::LashRuntime::builder(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
            lash_core::LeaseOwnerIdentity::opaque(
                "protocol-standard-test-worker",
                "protocol-standard-test-boot",
            ),
        )
        .with_session_id(session_id)
        .with_policy(policy)
        .with_runtime_host(host)
        .with_plugin_factories(factories)
        .build(),
    )
    .await
    .expect("runtime");

    let turn = runtime
        .stream_turn(
            lash_core::TurnInput::text("exercise discovery refusal"),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scoped_controller,
            ),
        )
        .await
        .expect("turn");

    assert!(matches!(
        turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "refusal continues the turn"
    );
    assert!(
        saw_refusal_text.load(Ordering::SeqCst),
        "the existing refusal text reaches the next model request"
    );
    assert_eq!(refused_executions.load(Ordering::SeqCst), 0);
    assert_eq!(
        admitted_executions.load(Ordering::SeqCst),
        usize::from(mixed)
    );
    assert_eq!(turn.tool_calls.len(), if mixed { 2 } else { 1 });
    let refused = turn
        .tool_calls
        .iter()
        .find(|record| record.call_id.as_deref() == Some("refused-call"))
        .expect("refused call is accounted");
    let lash_core::ToolCallOutcome::Failure(failure) = &refused.output.outcome else {
        panic!("refused call must remain a failure: {refused:?}");
    };
    assert_eq!(failure.code, "unknown_tool");
    assert_eq!(
        failure.message,
        "Tool `catalog_only` was not listed in this request; use a listed discovery operation or batch."
    );

    let entries = std::fs::read_to_string(&trace_path)
        .expect("read discovery trace")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("trace entry"))
        .collect::<Vec<_>>();
    for call_id in if mixed {
        vec!["refused-call", "admitted-call"]
    } else {
        vec!["refused-call"]
    } {
        assert_eq!(
            trace_lifecycle_count(&entries, call_id, "tool_call_started"),
            1,
            "one start for {call_id}: {entries:?}"
        );
        assert_eq!(
            trace_lifecycle_count(&entries, call_id, "tool_call_completed"),
            1,
            "one completion for {call_id}: {entries:?}"
        );
        let started = entries
            .iter()
            .position(|entry| {
                entry.get("type").and_then(Value::as_str) == Some("tool_call_started")
                    && entry.get("call_id").and_then(Value::as_str) == Some(call_id)
            })
            .expect("start position");
        let completed = entries
            .iter()
            .position(|entry| {
                entry.get("type").and_then(Value::as_str) == Some("tool_call_completed")
                    && entry.get("call_id").and_then(Value::as_str) == Some(call_id)
            })
            .expect("completion position");
        assert!(started < completed, "ordered lifecycle for {call_id}");
    }
    let _ = std::fs::remove_file(trace_path);
}

#[tokio::test]
async fn all_discovery_refusals_are_reported_and_accounted_before_continuing() {
    assert_discovery_refusal_is_reported_and_accounted(false).await;
}

#[tokio::test]
async fn mixed_discovery_refusals_and_admitted_calls_are_each_reported_once() {
    assert_discovery_refusal_is_reported_and_accounted(true).await;
}
