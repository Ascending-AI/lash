use super::*;
use lash_core::ToolCallOutcome;

async fn prepare(
    provider: &impl lash_core::ToolProvider,
    manifest: &lash_vocabulary::tool_contract::ToolManifest,
) -> lash_core::PreparedToolCall {
    let context = lash_core::ToolPrepareContext::for_testing(
        lash_vocabulary::identity::RuntimeOwner::Session("mcp-admitted-payload".into()),
        Arc::new(lash_core::testing::MockSessionManager::default()),
        None,
    );
    provider
        .prepare_tool_call(lash_core::ToolPrepareCall {
            tool_id: manifest.id.clone(),
            pending: lash_core::sansio::PendingToolCall {
                call_id: context.call_id().clone(),
                provider_call_id: None,
                tool_name: manifest.tool(),
                args: json!({}),
                replay: None,
            },
            context: &context,
        })
        .await
        .unwrap()
}

/// The context of an attempt prepared as `prepared`, through a link whose
/// codec payload is `payload`.
fn linked(
    prepared: &lash_core::PreparedToolCall,
    payload: Value,
) -> lash_core::testing::ToolCallFixture<'static> {
    lash_core::testing::ToolCallFixture::mock()
        .prepared_call(prepared)
        .execution_binding(Some(crate::plugin::mcp_binding(payload)))
}

/// C2's decided loosening: transport, peer and server version are instance
/// facts. An admitted call, `Once` included, is redelivered to the server
/// as it serves the tool now; its redeployed description changes no
/// contract, so nothing refuses it (OP-LINK-ONE).
#[tokio::test]
async fn mcp_redelivers_an_admitted_call_to_a_redeployed_server_that_kept_the_contract() {
    use lash_core::ToolProvider;
    let scratch = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        scratch.path(),
        MockOptions {
            behavior: "success",
            ..Default::default()
        },
    )
    .await;
    let provider = crate::McpToolProvider::new(Arc::clone(&pool));
    let config = lash_core::OwnerConfigView::default();
    let original = provider
        .tool_manifests(&config)
        .expect("fixture provider advertises")
        .remove(0);
    let (format, payload) = provider
        .link_payload(&original, &config)
        .expect("an MCP tool's link records the server it calls");
    assert_eq!(format, crate::MCP_BINDING_FORMAT);
    assert_eq!(
        payload,
        json!({"kind": "mcp", "server": "mock", "tool_id": original.id})
    );
    let pinned = pool.advertised_tools()[0].contract_id();
    let prepared = prepare(&provider, &original).await;
    let recorded = serde_json::to_value(&prepared).unwrap();
    {
        let mut catalog = pool.catalog.write_recover();
        let tool = catalog
            .servers
            .get_mut("mock")
            .unwrap()
            .tools
            .values_mut()
            .next()
            .unwrap();
        tool.definition.manifest.description = "Redeployed wording.".into();
    }
    assert_eq!(
        pool.advertised_tools()[0].contract_id(),
        pinned,
        "a description changes no contract"
    );
    let restored: lash_core::PreparedToolCall = serde_json::from_value(recorded).unwrap();
    let fixture = linked(&restored, payload);
    let context = fixture.attempt("mcp-redeployed-redelivery");
    let lash_core::ToolAttemptOutcome::Done { result, .. } = provider
        .execute(lash_core::ToolCall::new(
            &original,
            &crate::pool::result_contract_fixture(),
            &json!({}),
            &context,
        ))
        .await
    else {
        panic!("an inline MCP call finishes inline")
    };
    assert!(ToolOutcome::from_output(result.into_output()).is_success());
    pool.shutdown_all().await;
    assert_eq!(received(scratch.path()).matches("tools/call").count(), 1);
}

#[tokio::test]
async fn l02_mcp_redelivery_preserves_the_lash_call_identity_on_the_wire() {
    let scratch = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        scratch.path(),
        MockOptions {
            behavior: "success",
            ..Default::default()
        },
    )
    .await;
    let call_id = lash_core::ToolCallId::fixture("mcp-interrupted-attempt");
    let fixture = lash_core::testing::ToolCallFixture::mock().call_id(call_id.clone());
    let context = fixture.attempt("mcp-wire-law");
    let name = mcp_name("mock", "work");
    for _ in 0..2 {
        assert!(
            pool.call_tool(&name, &json!({}), &context)
                .await
                .is_success()
        );
    }
    pool.shutdown_all().await;
    let requests: Vec<serde_json::Value> =
        std::fs::read_to_string(scratch.path().join("received.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|request: &serde_json::Value| request["method"] == "tools/call")
            .collect();
    assert_eq!(requests.len(), 2);
    assert_ne!(
        requests[0]["id"], requests[1]["id"],
        "JSON-RPC only correlates a delivery"
    );
    for request in requests {
        assert_eq!(
            request["params"]["_meta"]["lash.dev/tool-call-id"],
            call_id.to_string()
        );
        assert_eq!(request["params"]["_meta"]["lash.dev/tool-attempt"], 1);
    }
}

/// A call runs only on the server its link's payload names: an absent or
/// corrupt payload, or one naming another tool, is refused before anything
/// is sent.
#[tokio::test]
async fn mcp_refuses_a_missing_or_corrupt_link_payload_before_send() {
    use lash_core::ToolProvider;
    let scratch = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        scratch.path(),
        MockOptions {
            behavior: "success",
            ..Default::default()
        },
    )
    .await;
    let provider = crate::McpToolProvider::new(Arc::clone(&pool));
    let original = provider
        .tool_manifests(&lash_core::OwnerConfigView::default())
        .expect("fixture provider advertises")
        .remove(0);
    let prepared = prepare(&provider, &original).await;
    let unbound = lash_core::testing::ToolCallFixture::mock().prepared_call(&prepared);
    let mut fixtures = vec![unbound];
    for payload in [
        Value::Null,
        json!({"server": "mock"}),
        json!({"kind": "mcp", "server": "mock", "tool_id": "mcp:mock/other"}),
    ] {
        fixtures.push(linked(&prepared, payload));
    }
    for fixture in &fixtures {
        let context = fixture.attempt("mcp-link-payload-refusal");
        let lash_core::ToolAttemptOutcome::Done { result, .. } = provider
            .execute(lash_core::ToolCall::new(
                &original,
                &crate::pool::result_contract_fixture(),
                &json!({}),
                &context,
            ))
            .await
        else {
            panic!("a payload refusal finishes inline")
        };
        assert_eq!(
            failure(&ToolOutcome::from_output(result.into_output())).code,
            "mcp_invalid_execution_binding"
        );
    }
    pool.shutdown_all().await;
    assert!(
        !received(scratch.path())
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .any(|request| request["method"] == "tools/call")
    );
}

#[tokio::test]
async fn l07_l08_mcp_inline_transports_refuse_defer_and_isolation_before_send() {
    use lash_core::ToolProvider;
    let scratch = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        scratch.path(),
        MockOptions {
            behavior: "success",
            ..Default::default()
        },
    )
    .await;
    let provider = crate::McpToolProvider::new(Arc::clone(&pool));
    let original = provider
        .tool_manifests(&lash_core::OwnerConfigView::default())
        .expect("fixture provider advertises")
        .remove(0);
    let prepared = prepare(&provider, &original).await;
    let fixture = linked(
        &prepared,
        json!({"kind": "mcp", "server": "mock", "tool_id": original.id}),
    );
    let context = fixture.attempt("mcp-unsupported-law");
    for mode in ["deferred", "isolated"] {
        let mut manifest = original.clone();
        match mode {
            "deferred" => {
                manifest = manifest
                    .declared(
                        lash_vocabulary::tool_declaration::ToolDeclaration::deferring(),
                        Some(lash_vocabulary::tool_contract::ParkBound::UntilScopeEnd),
                        None,
                    )
                    .expect("a deferring manifest with its park bound");
            }
            "isolated" => {
                manifest = manifest
                    .declared(
                        lash_vocabulary::tool_declaration::ToolDeclaration {
                            isolated: true,
                            ..lash_vocabulary::tool_declaration::ToolDeclaration::default()
                        },
                        None,
                        Some("sandbox".to_owned()),
                    )
                    .expect("an isolated manifest naming its engine");
            }
            _ => unreachable!("only the two declared unsupported modes"),
        }
        let attempt = provider
            .execute(lash_core::ToolCall::new(
                &manifest,
                &crate::pool::result_contract_fixture(),
                &json!({}),
                &context,
            ))
            .await;
        let lash_core::ToolAttemptOutcome::Done { result, .. } = attempt else {
            panic!("unsupported work never becomes Deferred")
        };
        let result = ToolOutcome::from_output(result.into_output());
        assert_eq!(failure(&result).code, "mcp_unsupported_remote_completion");
    }
    let grant = crate::McpDeferredToolProvider::new(Arc::clone(&pool));
    let wrong_server = lash_core::testing::mock_attempt_context_with_execution_binding(
        crate::plugin::mcp_binding(
            json!({"kind": "mcp", "tool_id": original.id, "server": "other"}),
        ),
    );
    let attempt = grant
        .execute(lash_core::ToolCall::new(
            &original,
            &crate::pool::result_contract_fixture(),
            &json!({}),
            &wrong_server,
        ))
        .await;
    let lash_core::ToolAttemptOutcome::Done { result, .. } = attempt else {
        panic!("a grant binds its server")
    };
    assert_eq!(
        failure(&ToolOutcome::from_output(result.into_output())).code,
        "mcp_invalid_execution_binding"
    );
    pool.shutdown_all().await;
    assert!(!received(scratch.path()).contains("tools/call"));
}

#[tokio::test]
async fn l03_mcp_cancellation_never_claims_remote_success_or_termination() {
    let scratch = tempfile::tempdir().unwrap();
    let clock = scripted::Clock::new().await;
    let (pool, mut mock) = scripted::Mock::connect(scratch.path(), MockOptions::default()).await;
    let token = tokio_util::sync::CancellationToken::new();
    let fixture =
        lash_core::testing::ToolCallFixture::mock().cancellation_token(Some(token.clone()));
    let context = fixture.attempt("mcp-cancel-law");
    let name = mcp_name("mock", "work");
    let args = json!({});
    let call = pool.call_tool(&name, &args, &context);
    tokio::pin!(call);
    assert!(futures_util::poll!(call.as_mut()).is_pending());
    mock.started(&pool).await;
    token.cancel();
    let outcome = call.await;
    mock.event("cancelled").await;
    mock.command("reply").await;
    peer(&pool)
        .await
        .send_request(rmcp::model::CustomRequest::new("fixture/barrier", None).into())
        .await
        .unwrap();
    assert!(matches!(
        outcome.as_done_output().unwrap().outcome,
        ToolCallOutcome::Cancelled(_)
    ));
    assert!(matches!(
        pool.call_tool(&name, &json!({}), &context)
            .await
            .as_done_output()
            .unwrap()
            .outcome,
        ToolCallOutcome::Cancelled(_)
    ));
    drop(clock);
    pool.shutdown_all().await;
    mock.event("eof").await;
}

#[tokio::test]
async fn l22_mcp_transport_timeout_is_an_inline_body_failure() {
    use lash_core::ToolProvider;
    let scratch = tempfile::tempdir().unwrap();
    let clock = scripted::Clock::new().await;
    let (pool, mut mock) = scripted::Mock::connect(scratch.path(), MockOptions::default()).await;
    let provider = crate::McpToolProvider::new(Arc::clone(&pool));
    let manifest = provider
        .tool_manifests(&lash_core::OwnerConfigView::default())
        .expect("fixture provider advertises")
        .remove(0);
    let prepared = prepare(&provider, &manifest).await;
    let (_, payload) = provider
        .link_payload(&manifest, &lash_core::OwnerConfigView::default())
        .expect("the tool's link payload");
    let fixture = linked(&prepared, payload);
    let context = fixture.attempt("mcp-body-timeout-law");
    let args = json!({});
    let contract = crate::pool::result_contract_fixture();
    let request = provider.execute(lash_core::ToolCall::new(
        &manifest, &contract, &args, &context,
    ));
    tokio::pin!(request);
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    mock.started(&pool).await;
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    tokio::time::advance(Duration::from_millis(150) + scripted::TIMER_TICK).await;
    let lash_core::ToolAttemptOutcome::Done { result, intents } = request.await else {
        panic!("a body transport timeout supplies no durable source or process");
    };
    assert!(intents.is_empty());
    let result = ToolOutcome::from_output(result.into_output());
    assert_eq!(failure(&result).class, ToolFailureClass::Timeout);
    assert_eq!(failure(&result).code, "mcp_call_timeout");
    assert_eq!(
        failure(&result).raw.as_ref().unwrap().to_json_value()["kind"],
        "call_timeout"
    );
    mock.event("cancelled").await;
    drop(clock);
    pool.shutdown_all().await;
}

/// The one `tools/call` request the mock received.
fn received_call(root: &std::path::Path) -> serde_json::Value {
    let calls: Vec<serde_json::Value> = std::fs::read_to_string(root.join("received.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|request: &serde_json::Value| request["method"] == "tools/call")
        .collect();
    assert_eq!(calls.len(), 1);
    calls.into_iter().next().unwrap()
}

/// L13 and L9's MCP rows: a server at the default `W3c` policy receives
/// `_meta.traceparent` equal to the context of the attempt's tool span
/// (trace id, span id and flags; `tracestate` when it has one), and one
/// configured `off` receives exactly the `lash.dev/*` keys beside the
/// protocol's own.
/// Either way the client annotates the tool span with the MCP method, the
/// transport and the JSON-RPC request id, and an `isError` result with
/// `error.type = tool_error`.
#[tokio::test]
async fn l13_mcp_propagates_the_tool_span_context_under_the_servers_policy() {
    use crate::McpTracePropagation;
    for propagation in [McpTracePropagation::W3c, McpTracePropagation::Off] {
        let scratch = tempfile::tempdir().unwrap();
        let pool = connect_mock(
            scratch.path(),
            MockOptions {
                behavior: "tool_error",
                trace_propagation: propagation,
                ..Default::default()
            },
        )
        .await;
        let runtime =
            lash_core::trace::TraceRuntime::new(Arc::new(lash_core::facade_support::SystemClock))
                .with_trace_sink(Arc::new(lash_core::testing::RecordingTraceSink::new()));
        let bound = lash_core::testing::bound_tool_trace(
            &runtime,
            &lash_core::plugin::PluginCallbackIdentity {
                owner: "mcp".to_string(),
                key: "tool_provider:mcp".to_string(),
            },
        );
        let fixture = lash_core::testing::ToolCallFixture::mock();
        let context = bound.on(fixture.attempt("mcp-trace-law"));
        let outcome = pool
            .call_tool(&mcp_name("mock", "work"), &json!({}), &context)
            .await;
        assert!(!outcome.is_success(), "the server answered isError");
        pool.shutdown_all().await;

        let request = received_call(scratch.path());
        let meta = request["params"]["_meta"].as_object().expect("a _meta");
        let position = bound.trace().position().expect("a traced body");
        // The keys beside the protocol's own, which its transport adds.
        let added: std::collections::BTreeSet<&str> = meta
            .keys()
            .map(String::as_str)
            .filter(|key| !key.starts_with("io.modelcontextprotocol/") && *key != "progressToken")
            .collect();
        let mut expected =
            std::collections::BTreeSet::from(["lash.dev/tool-attempt", "lash.dev/tool-call-id"]);
        if propagation == McpTracePropagation::W3c {
            expected.insert("traceparent");
            if !position.tracestate().is_empty() {
                expected.insert("tracestate");
            }
        }
        assert_eq!(added, expected, "{meta:?}");
        match propagation {
            McpTracePropagation::W3c => {
                assert_eq!(meta["traceparent"], position.traceparent());
                assert_eq!(
                    meta.get("tracestate").and_then(Value::as_str),
                    (!position.tracestate().is_empty()).then(|| position.tracestate().as_str()),
                );
            }
            McpTracePropagation::Off => {}
        }
        let annotations = bound.tool_span_annotations();
        let value = |key: &str| {
            annotations
                .get(key)
                .map(|value| format!("{value:?}"))
                .unwrap_or_else(|| panic!("{key} is set: {annotations:?}"))
        };
        assert!(value("mcp.method.name").contains("tools/call"));
        assert!(value("network.transport").contains("pipe"));
        assert!(value("error.type").contains("tool_error"));
        assert!(value("jsonrpc.request.id").contains(&request["id"].to_string()));
        Box::pin(process_mcp_trace(propagation)).await;
    }
}

/// L13's round trip: a server configured `off` keeps it, and the default
/// is omitted.
#[test]
fn l13_mcp_trace_propagation_round_trips_and_omits_the_default() {
    let root = std::path::Path::new("/nonexistent");
    for (propagation, written) in [
        (crate::McpTracePropagation::W3c, None),
        (crate::McpTracePropagation::Off, Some(json!("off"))),
    ] {
        let config = mock_config(
            root,
            MockOptions {
                trace_propagation: propagation,
                ..Default::default()
            },
        );
        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(value.get("trace_propagation").cloned(), written);
        let back: McpServerConfig = serde_json::from_value(value).unwrap();
        assert_eq!(back.trace_propagation, propagation);
    }
}

struct McpTraceComponent(Arc<crate::McpToolProvider>);

impl lash_core::plugin::ProcessComponentFactory for McpTraceComponent {
    fn declaration(&self) -> lash_core::plugin::ProcessComponentDeclaration {
        lash_core::plugin::ProcessComponentDeclaration::initial(
            "mcp",
            lash_kernel_value::providers::FormatVersion::ONE,
        )
        .unwrap()
    }
    fn config_type(&self) -> std::any::TypeId {
        std::any::TypeId::of::<lash_tool_support::ToolSelectionConfig>()
    }
    fn validate(
        &self,
        entry: &lash_core::ProcessComponentEntry,
        _: &lash_core::ProcessEnv,
    ) -> Result<(), lash_core::ProcessEnvRefusal> {
        entry
            .config()
            .decode::<lash_tool_support::ToolSelectionConfig>()
            .map(|_| ())
            .map_err(|error| lash_core::ProcessEnvRefusal::ConfigRefused {
                id: entry.id().to_string(),
                reason: error.to_string(),
            })
    }
    fn recipe(
        self: Arc<Self>,
    ) -> Result<lash_core::plugin::ProcessRecipe, lash_execution_contracts::PluginError> {
        lash_core::plugin::ProcessComponentSpec::new()
            .with_tool_provider(lash_core::tool_provider_key!("servers"), self.0.clone())
            .with_package_provider(lash_core::tool_provider_key!("servers"), self.0.clone())
            .recipe("mcp")
    }
}

/// Run the same MCP attempt through the production process owner, whose
/// acknowledged outcome must close the body span with its annotations.
async fn process_mcp_trace(propagation: crate::McpTracePropagation) {
    use lash::package::*;
    let scratch = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        scratch.path(),
        MockOptions {
            behavior: "tool_error",
            trace_propagation: propagation,
            ..Default::default()
        },
    )
    .await;
    let provider = Arc::new(crate::McpToolProvider::new(Arc::clone(&pool)));
    let definition = pool.advertised_tools().remove(0);
    let component_key = lash_core::ProcessConfigKey::<lash_tool_support::ToolSelectionConfig>::new(
        "mcp",
        lash_kernel_value::providers::FormatVersion::ONE,
    );
    let component = Arc::new(McpTraceComponent(provider));
    let stores = Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap());
    let backend = lash::durable::DurableBackendBuilder::new(stores)
        .config(lash::durable::DurableSettings::standard())
        .build()
        .unwrap();
    let workers = lash::vm::WorkerService::default();
    let bounds = workers.config().run_bounds;
    let sink = Arc::new(lash_core::testing::RecordingTraceSink::new());
    let core = lash::LashCore::standard_builder(backend)
        .plugin(Arc::new(lash::vm::KernelProcessPluginFactory::new(
            workers,
            "typescript",
            bounds,
        )))
        .process_component(component, component_key)
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .trace_sink(sink.clone())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("mcp-process-trace"),
            lash::persistence::LeaseIncarnationId::new("mcp-process-trace-boot"),
        ))
        .unwrap();
    let env = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(16))
        .component(
            component_key,
            &lash_tool_support::ToolSelectionConfig::default(),
        )
        .component(lash::vm::KERNEL_SERVICES_CONFIG, &())
        .build()
        .unwrap();
    let artifacts = core.host_artifacts();
    let pin = lash_core::HostArtifactPin::mint();
    let context = PublicationContext {
        domain: PublicationDomain(core.backend().binding_identity().to_string()),
        scope: PublicationScope::Host,
        publisher: PublisherInstance("mcp-trace-law".into()),
        root: Segment::new("law").unwrap(),
    };
    let packages = artifacts
        .process_namespace(&pin, &env, context.clone())
        .await
        .unwrap();
    let environment = lash::vm::WorkflowEnvironmentRequest {
        snapshot: packages.snapshot.clone(),
        context,
        attachment: Attachment {
            packages: packages
                .snapshot
                .advertisements
                .iter()
                .filter_map(|accepted| match &accepted.advertisement {
                    PackageAdvertisement::Enumerated(package) => Some(package.path.clone()),
                    _ => None,
                })
                .collect(),
            members: Default::default(),
            deferred: Default::default(),
        },
    };
    let mut document = lash::workflow::document::parse_document(&format!("kernel 1\nimport 0 operation {}(input: Any) -> Any\nmain {{ let got = perform 0({{}}) as Any return got }}", definition.manifest.operation.effect_name())).unwrap();
    for import in document.imports.values_mut() {
        if let lash_kernel_value::ImportKind::Operation { signature } = &mut import.kind {
            *signature = lash_kernel_value::json_schema::tool_signature(
                &definition.contract.input_schema.canonical,
                &definition.contract.output_schema.canonical,
            );
        }
    }
    let start = artifacts
        .publish_workflow(
            &pin,
            &lash::workflow::document::DocumentBytes::from(document),
            &StartSelector::Main,
            &environment,
            None,
            None,
        )
        .await
        .unwrap();
    let env = artifacts
        .link_process_env(
            &env,
            &lash::process::LinkTarget::Linked(start.clone()),
            &packages,
            &Default::default(),
        )
        .await
        .unwrap();
    let env_ref = artifacts.publish_process_env(&pin, &env).await.unwrap();
    let payload = json!({"code": lash::vm::KernelInlineRequest::Linked { start }, "args":{}});
    let engine = lash_protocol_types::LASH_VM_ENGINE_KIND;
    let settings = core.processes().engine_defaults(engine, &payload).unwrap();
    let request = lash::process::ProcessStartRequest::engine(
        engine,
        payload,
        lash::process::EngineLaunch::new(env_ref, settings),
        lash::process::ProcessOriginator::host(),
        lash::process::LifetimeDecision::Detached,
    );
    let process = core
        .processes()
        .start(lash::OperationId::mint(), request)
        .await
        .unwrap()
        .process_id;
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        core.processes().await_output(&process),
    )
    .await
    .unwrap()
    .unwrap();
    core.shutdown().await.unwrap();
    pool.shutdown_all().await;
    let request = received_call(scratch.path());
    let records = sink.json_records().unwrap();
    let completed: Vec<_> = records
        .iter()
        .filter(|record| {
            record["type"] == "tool_call_completed"
                && record["context"]["process_id"] == process.as_str()
        })
        .collect();
    assert_eq!(
        completed.len(),
        1,
        "the committing process closes its call once: {records:?}"
    );
    let completed = completed[0];
    let attributes = completed["span_attributes"]
        .as_array()
        .expect("the process closer keeps body annotations");
    let value = |key: &str| {
        attributes
            .iter()
            .find(|attribute| attribute["key"] == key)
            .unwrap_or_else(|| panic!("{key} is set: {attributes:?}"))["value"]
            .to_string()
    };
    assert!(value("mcp.method.name").contains("tools/call"));
    assert!(value("network.transport").contains("pipe"));
    assert!(value("error.type").contains("tool_error"));
    assert!(value("jsonrpc.request.id").contains(&request["id"].to_string()));
    match propagation {
        crate::McpTracePropagation::W3c => {
            let parent = request["params"]["_meta"]["traceparent"]
                .as_str()
                .expect("a process propagates its body span");
            let fields: Vec<_> = parent.split('-').collect();
            assert_eq!(fields.len(), 4);
            let span = completed["position"]["span"].to_string();
            assert!(
                span.contains(fields[1]) && span.contains(fields[2]),
                "the propagated parent is the exported process call span: {completed:?} {parent}"
            );
        }
        crate::McpTracePropagation::Off => {
            assert!(request["params"]["_meta"].get("traceparent").is_none())
        }
    }
}
