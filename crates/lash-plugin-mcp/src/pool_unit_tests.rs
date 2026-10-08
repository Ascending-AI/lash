// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use lash_core::ToolProvider;
use lash_sansio::sync::{MutexExt, RwLockExt};

#[tokio::test]
async fn mcp_view_preserves_order_and_filters_nonassistant_blocks() {
    let result = serde_json::from_value(json!({
        "content": [
            {"type":"text","text":"private","annotations":{"audience":["user"]}},
            {"type":"text","text":"public"},
            {"type":"resource_link","uri":"file:///report","name":"report","mimeType":"text/plain","annotations":{"priority":0.7,"lastModified":"2026-09-28T00:00:00Z"}},
            {"type":"resource","resource":{"uri":"file:///source","text":"body"}}
        ],
        "structuredContent":{"answer":42}
    })).expect("valid MCP result");
    let output = tool_result_from_rmcp(result, &lash_core::testing::mock_attempt_context())
        .await
        .into_done_output()
        .expect("settled");
    assert_eq!(
        output.value_for_projection(),
        json!({"structuredContent":{"answer":42},"content":[{"type":"text","text":"public"},{"type":"resource_link","uri":"file:///report","name":"report","mimeType":"text/plain"},{"type":"resource","uri":"file:///source","text":"body"}]})
    );
    let blocks = &output.view.expect("authored view").blocks;
    assert_eq!(blocks.len(), 3);
    assert!(matches!(&blocks[0], ToolViewBlock::Text { text, .. } if text == "public"));
    assert!(
        matches!(&blocks[1], ToolViewBlock::ResourceLink { uri, name, mime_type, meta, .. }
        if uri == "file:///report" && name == "report" && mime_type.as_deref() == Some("text/plain")
            && meta.priority.is_some_and(|priority| (priority - 0.7).abs() < 0.000001)
            && meta.last_modified.as_deref() == Some("2026-09-28T00:00:00+00:00")),
        "{:?}",
        blocks[1]
    );
    assert!(
        matches!(&blocks[2], ToolViewBlock::Text { text, .. } if text == "file:///source\nbody")
    );
}

#[tokio::test]
async fn mcp_json_copy_uses_the_structured_value_without_a_view() {
    let result = serde_json::from_value(json!({
        "content":[{"type":"text","text":"{\"answer\":42}"}],
        "structuredContent":{"answer":42}
    }))
    .expect("valid MCP result");
    let output = tool_result_from_rmcp(result, &lash_core::testing::mock_attempt_context())
        .await
        .into_done_output()
        .expect("settled");
    assert_eq!(output.value_for_projection(), json!({"answer":42}));
    assert!(
        matches!(&output.outcome, lash_core::ToolCallOutcome::Success(value)
        if value.to_json_value() == json!({"structuredContent":{"answer":42},"content":[]}))
    );
    assert!(output.view.is_none());
}

#[test]
fn imported_mcp_tools_declare_the_fixed_result_envelope() {
    let with_schema: rmcp::model::Tool = serde_json::from_value(json!({
        "name":"lookup", "inputSchema":{"type":"object"},
        "outputSchema":{"type":"object","properties":{"answer":{"$ref":"#/$defs/Answer"}},
            "$defs":{"Answer":{"type":"integer"}}}
    }))
    .expect("tool");
    for field in ["inputSchema", "outputSchema"] {
        let mut defective = serde_json::to_value(&with_schema).expect("encode tool");
        defective[field] = json!({"type":"unknown"});
        let defective = serde_json::from_value(defective).expect("wire tool");
        let error = import_tools("test", vec![defective], std::time::Duration::from_secs(30))
            .err()
            .expect("unusable schema refused at discovery");
        assert!(matches!(
            error,
            McpError::UnusableSchema(
                lash_core::facade_support::ToolCatalogBuildError::UnusableSchema {
                    source: lash_core::SchemaAdmissionError::Compilation { .. },
                    ..
                }
            )
        ));
    }
    let source = ToolDefinition::raw("bad", "bad", "bad", Value::Null, json!({}))
        .expect_err("invalid schema");
    let fault = McpServerFault::UnusableSchema(Box::new(source.clone()));
    for health in [
        McpServerHealth::Connected {
            catalog_error: Some(fault.clone()),
        },
        McpServerHealth::Reconnecting {
            last_error: Some(fault.clone()),
        },
        McpServerHealth::Exhausted {
            attempts: 3,
            last_error: Some(fault.clone()),
        },
        McpServerHealth::ShuttingDown {
            reason: Some(fault.clone()),
        },
    ] {
        let restored: McpServerHealth =
            serde_json::from_value(serde_json::to_value(&health).expect("encode health"))
                .expect("restore health");
        assert_eq!(restored.fault(), Some(&fault));
        let failure =
            lash_core::ToolFailure::from(crate::call_failure::McpCallFailure::ServerUnavailable {
                server: "test".into(),
                health: restored,
                after_ms: 0,
            });
        assert!(matches!(failure.cause.as_deref(),
            Some(lash_core::ToolFailureCause::ToolSchemaAdmission { source: retained }) if retained.as_ref() == &source));
    }
    let without_schema = advertised_tool("plain");
    let tools = import_tools(
        "test",
        vec![with_schema, without_schema],
        std::time::Duration::from_secs(30),
    )
    .expect("imports");
    for tool in tools.values() {
        let schema = tool.definition.contract.output_schema.canonical.as_value();
        assert_eq!(schema["required"], json!(["content"]));
        assert_eq!(schema["properties"]["content"]["type"], "array");
        assert_eq!(
            schema["properties"]["content"]["items"]["oneOf"]
                .as_array()
                .map(Vec::len),
            Some(4)
        );
        tool.definition
            .contract
            .output_schema
            .canonical
            .validate(&json!({"content": [], "structuredContent": {"answer": 42}}))
            .expect("embedded refs validate against the server resource");
        if tool.original_name == "lookup" {
            let mismatch = tool
                .definition
                .contract
                .output_schema
                .canonical
                .validate(&json!({"content": [], "structuredContent": {"answer": "wrong"}}))
                .expect_err("embedded constraints remain active");
            assert_eq!(mismatch.instance_path, "/structuredContent/answer");
            assert_eq!(
                schema["properties"]["structuredContent"]["properties"]["answer"]["$ref"],
                "#/$defs/Answer"
            );
        } else {
            assert_eq!(schema["properties"]["structuredContent"], json!({}));
        }
    }
}

#[tokio::test]
async fn mcp_json_copy_stays_in_mixed_view_but_not_code_envelope() {
    let result = serde_json::from_value(json!({
        "content":[
            {"type":"text","text":"before"},
            {"type":"text","text":"{\"answer\":42}"},
            {"type":"text","text":"after"}
        ],
        "structuredContent":{"answer":42}
    }))
    .expect("valid result");
    let output = tool_result_from_rmcp(result, &lash_core::testing::mock_attempt_context())
        .await
        .into_done_output()
        .expect("settled");
    assert_eq!(
        output.value_for_projection(),
        json!({
            "structuredContent":{"answer":42},
            "content":[{"type":"text","text":"before"},{"type":"text","text":"after"}]
        })
    );
    assert_eq!(
        output.view.expect("view").blocks,
        ["before", "{\"answer\":42}", "after"].map(|text| lash_core::ToolViewBlock::Text {
            text: text.into(),
            meta: Default::default(),
        })
    );
}

#[tokio::test]
async fn mcp_user_only_content_has_an_empty_assistant_view() {
    let result = serde_json::from_value(json!({
        "content":[{"type":"text","text":"private","annotations":{"audience":["user"]}}]
    }))
    .expect("valid result");
    let output = tool_result_from_rmcp(result, &lash_core::testing::mock_attempt_context())
        .await
        .into_done_output()
        .expect("settled");
    assert_eq!(output.value_for_projection(), json!({"content":[]}));
    assert!(output.view.expect("empty assistant view").blocks.is_empty());
}

fn mcp_name(server: &str, native_tool: &str) -> String {
    crate::mcp_tool_names(server, &[native_tool])[native_tool].clone()
}

/// Drive a provider through the single `execute` seam by resolving its
/// manifest for `tool_id` first, then project the attempt outcome to the
/// plain outcome these assertions inspect. The projection asserts the call
/// declared no leaf intents.
async fn execute_by_id<P: ToolProvider>(
    provider: &P,
    tool_id: &lash_core::ToolId,
    args: &Value,
    context: &lash_core::AttemptContext<'_>,
) -> ToolOutcome {
    let manifest = provider
        .resolve_manifest_by_id(tool_id)
        .expect("manifest resolves for tool id");
    execute_with_manifest(provider, &manifest, args, context).await
}

/// Drive a provider through the single `execute` seam with an already-pinned
/// manifest, for tools dropped from the catalog that must still reject through
/// the typed unknown-id path.
async fn execute_with_manifest<P: ToolProvider>(
    provider: &P,
    manifest: &lash_core::ToolManifest,
    args: &Value,
    context: &lash_core::AttemptContext<'_>,
) -> ToolOutcome {
    match provider
        .execute(lash_core::ToolCall::new(manifest, args, context))
        .await
    {
        lash_core::ToolAttemptOutcome::Done { result, intents } => {
            assert!(
                intents.is_empty(),
                "test leaf execution declares no intents"
            );
            ToolOutcome::from_output(result.into_output())
        }
        lash_core::ToolAttemptOutcome::HostFailed(error) => {
            panic!("unexpected host fault: {error}")
        }
        lash_core::ToolAttemptOutcome::Pending(pending) => ToolOutcome::Pending(Box::new(pending)),
    }
}

fn forced_publication_name() -> (String, lash_tool_support::ToolBinding) {
    (
        "mcp__forced__collision".to_string(),
        lash_tool_support::ToolBinding::new(["forced"], "collision"),
    )
}

fn advertised_tool(name: &str) -> rmcp::model::Tool {
    serde_json::from_value(json!({
        "name": name,
        "inputSchema": { "type": "object" }
    }))
    .expect("valid MCP tool fixture")
}

#[test]
fn import_refuses_a_forced_final_name_collision_without_overwriting() {
    let names =
        naming::build_catalog_names_with_digest("directory", &["get-user", "get_user"], |_| [7; 5]);
    let result = import_tools_with_name_builder(
        "directory",
        vec![advertised_tool("get-user"), advertised_tool("get_user")],
        std::time::Duration::from_secs(30),
        |_, tool| names[tool].clone(),
    );
    let error = match result {
        Ok(_) => panic!("colliding final names must be refused"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(message.contains("model-facing name collision"), "{message}");
    assert!(message.contains("get-user"), "{message}");
    assert!(message.contains("get_user"), "{message}");
}

#[tokio::test]
async fn replacement_publication_survives_old_cleanup_and_refuses_stale_actor() {
    let pool = Arc::new(McpConnectionPool::empty());
    let server_name = "abcdefghijklmno-one";
    let forced_catalog = |server: &str, tool: &str| {
        import_tools_with_name_builder(
            server,
            vec![advertised_tool(tool)],
            std::time::Duration::from_secs(30),
            |_, _| forced_publication_name(),
        )
        .expect("one-tool forced catalog")
    };
    let old = McpEntry::new_with_publication_state(
        Arc::clone(&pool.publication_state),
        server_name.to_string(),
        McpServerConfig::stdio(McpStdioTransport::new("sh", Vec::new())),
        McpHostServices::default(),
    );
    pool.install(old.server_name.clone(), Arc::clone(&old))
        .unwrap_or_else(|(_, error)| panic!("install old entry: {error}"));
    old.replace_imported_tools(forced_catalog(server_name, "abcdefghijklmnop"))
        .expect("old entry publishes while current");

    let replacement = McpEntry::new_with_publication_state(
        Arc::clone(&pool.publication_state),
        server_name.to_string(),
        McpServerConfig::stdio(McpStdioTransport::new("sh", Vec::new())),
        McpHostServices::default(),
    );
    let removed = pool
        .install(replacement.server_name.clone(), Arc::clone(&replacement))
        .unwrap_or_else(|(_, error)| panic!("install replacement entry: {error}"))
        .expect("old entry replaced");
    replacement
        .replace_imported_tools(forced_catalog(server_name, "abcdefghijklmnop"))
        .expect("replacement entry publishes");

    pool.retire_publication(&removed);
    let advertised = pool.advertised_tools();
    assert_eq!(advertised.len(), 1);
    assert_eq!(
        advertised[0].manifest.id.as_str(),
        "mcp:19:abcdefghijklmno-one/16:abcdefghijklmnop"
    );

    let stale_error = old
        .replace_imported_tools(forced_catalog(server_name, "different-native"))
        .expect_err("removed actor must not republish a ghost catalog");
    assert!(
        stale_error.to_string().contains("stale tool publication"),
        "{stale_error}"
    );

    let contender = McpEntry::new_with_publication_state(
        Arc::clone(&pool.publication_state),
        "abcdefghijklmno-two".to_string(),
        McpServerConfig::stdio(McpStdioTransport::new("sh", Vec::new())),
        McpHostServices::default(),
    );
    pool.install(contender.server_name.clone(), Arc::clone(&contender))
        .unwrap_or_else(|(_, error)| panic!("install contender entry: {error}"));
    let collision = contender
        .replace_imported_tools(forced_catalog(&contender.server_name, "abcdefghijklmnop"))
        .expect_err("replacement reservation must refuse a forced collision");
    assert!(
        collision
            .to_string()
            .contains("model-facing name collision"),
        "{collision}"
    );
    assert_eq!(pool.advertised_tools().len(), 1);
    assert_eq!(pool.publication_state.lock_recover().tool_names.len(), 1);

    removed.shutdown().await;
    pool.shutdown_all().await;
}

#[tokio::test]
async fn advertised_tools_snapshot_never_combines_colliding_catalog_generations() {
    let pool = Arc::new(McpConnectionPool::empty());
    let first = McpEntry::new_with_publication_state(
        Arc::clone(&pool.publication_state),
        "abcdefghijklmno-one".to_string(),
        McpServerConfig::stdio(McpStdioTransport::new("sh", Vec::new())),
        McpHostServices::default(),
    );
    let second = McpEntry::new_with_publication_state(
        Arc::clone(&pool.publication_state),
        "abcdefghijklmno-two".to_string(),
        McpServerConfig::stdio(McpStdioTransport::new("sh", Vec::new())),
        McpHostServices::default(),
    );
    pool.install(first.server_name.clone(), Arc::clone(&first))
        .unwrap_or_else(|(_, error)| panic!("install first server: {error}"));
    pool.install(second.server_name.clone(), Arc::clone(&second))
        .unwrap_or_else(|(_, error)| panic!("install second server: {error}"));
    let forced_catalog = |server: &str| {
        import_tools_with_name_builder(
            server,
            vec![advertised_tool("abcdefghijklmnop")],
            std::time::Duration::from_secs(30),
            |_, _| forced_publication_name(),
        )
        .expect("one-tool catalog")
    };
    first
        .replace_imported_tools(forced_catalog(&first.server_name))
        .expect("first catalog publishes");

    let snapshot_paused = Arc::new(std::sync::Barrier::new(2));
    let snapshot_released = Arc::new(std::sync::Barrier::new(2));
    let hook_paused = Arc::clone(&snapshot_paused);
    let hook_released = Arc::clone(&snapshot_released);
    *pool.advertised_tools_hook.write_recover() = Some(Arc::new(move || {
        hook_paused.wait();
        hook_released.wait();
    }));

    let reader_pool = Arc::clone(&pool);
    let reader = std::thread::spawn(move || reader_pool.advertised_tools());
    snapshot_paused.wait();

    let (writer_started_tx, writer_started_rx) = std::sync::mpsc::channel();
    let (writer_done_tx, writer_done_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        writer_started_tx.send(()).expect("signal writer start");
        first
            .replace_imported_tools(BTreeMap::new())
            .expect("first catalog retires its name");
        second
            .replace_imported_tools(forced_catalog(&second.server_name))
            .expect("second catalog acquires the released name");
        writer_done_tx.send(()).expect("signal writer completion");
        (first, second)
    });
    writer_started_rx.recv().expect("writer started");
    assert!(
        writer_done_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err(),
        "catalog transfer must wait until the aggregate snapshot releases publication"
    );

    snapshot_released.wait();
    let snapshot = reader.join().expect("snapshot reader joins");
    writer_done_rx.recv().expect("writer completes");
    let (first, second) = writer.join().expect("catalog writer joins");
    *pool.advertised_tools_hook.write_recover() = None;

    assert_eq!(snapshot.len(), 1);
    assert_eq!(pool.advertised_tools().len(), 1);
    assert!(first.imported_tools.read_recover().is_empty());
    assert_eq!(second.imported_tools.read_recover().len(), 1);

    pool.shutdown_all().await;
}

#[tokio::test]
async fn roots_notification_failures_are_aggregated_after_every_attempt() {
    let attempts = Arc::new(AtomicU64::new(0));
    let failures = collect_notification_failures(
        vec![
            ("alpha".to_string(), Some("offline")),
            ("bravo".to_string(), None),
            ("charlie".to_string(), Some("closed")),
        ],
        |failure| {
            let attempts = Arc::clone(&attempts);
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                failure.map_or(Ok(()), Err)
            }
        },
    )
    .await;

    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(failures, ["`alpha`: offline", "`charlie`: closed"]);
}

/// Regression for the header-drop bug: custom/auth headers configured for
/// an HTTP MCP server must be translated into the `http` header types the
/// transport actually sends. Before the fix, `connect_service` called
/// `from_uri` and dropped the configured `headers` map entirely, so an
/// `Authorization` header never reached the server.
#[test]
fn build_http_headers_carries_configured_headers() {
    let mut headers = BTreeMap::new();
    headers.insert(
        "Authorization".to_string(),
        "Bearer secret-token".to_string(),
    );
    headers.insert("X-Tenant".to_string(), "acme".to_string());

    let built = build_http_headers("api", &headers).expect("valid headers convert");

    assert_eq!(
        built
            .get(&HeaderName::from_static("authorization"))
            .map(|v| v.to_str().unwrap()),
        Some("Bearer secret-token"),
        "configured Authorization header must be carried through to the transport"
    );
    assert_eq!(
        built
            .get(&HeaderName::from_static("x-tenant"))
            .map(|v| v.to_str().unwrap()),
        Some("acme")
    );
    assert_eq!(built.len(), 2);
}

#[test]
fn build_http_headers_rejects_malformed_name() {
    let mut headers = BTreeMap::new();
    headers.insert("Bad Header Name".to_string(), "x".to_string());
    let err = build_http_headers("api", &headers).expect_err("malformed name rejected");
    assert!(
        matches!(err, McpError::Config(_)),
        "expected a config error, got {err:?}"
    );
}

#[test]
fn build_http_headers_rejects_malformed_value() {
    let mut headers = BTreeMap::new();
    // A newline is not a legal header value byte.
    headers.insert("X-Bad".to_string(), "line1\nline2".to_string());
    let err = build_http_headers("api", &headers).expect_err("malformed value rejected");
    assert!(
        matches!(err, McpError::Config(_)),
        "expected a config error, got {err:?}"
    );
}

/// A server that is down at startup must not fail pool construction: the
/// entry stays registered (status: disconnected, with the error recorded)
/// and only configuration errors abort.
#[tokio::test]
async fn connect_tolerates_unreachable_server() {
    let mut servers = BTreeMap::new();
    servers.insert(
        "down".to_string(),
        McpServerConfig {
            startup_timeout_ms: 1_000,
            call_policy: McpCallPolicy {
                call_timeout_ms: 1_000,
                ..Default::default()
            },
            shutdown_policy: Default::default(),
            transport: McpTransport::Stdio(McpStdioTransport {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), "exit 1".to_string()],
                env: BTreeMap::new(),
                cwd: None,
            }),
        },
    );

    let pool = McpConnectionPool::connect(servers)
        .await
        .expect("an unreachable server must not fail pool construction");

    assert!(pool.advertised_tools().is_empty());
    let statuses = pool.server_statuses();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].server_name, "down");
    assert!(!statuses[0].health.is_connected());
    assert!(
        statuses[0].health.error().is_some(),
        "the connection failure is recorded for observability"
    );

    let unknown_name = mcp_name("down", "anything");
    let result = pool
        .call_tool(
            &unknown_name,
            &json!({}),
            &lash_core::testing::mock_attempt_context(),
        )
        .await;
    assert!(!result.is_success(), "calls fail loudly while disconnected");

    pool.shutdown_all().await;

    let result = pool
        .call_tool(
            &unknown_name,
            &json!({}),
            &lash_core::testing::mock_attempt_context(),
        )
        .await;
    let output = result
        .as_done_output()
        .expect("post-shutdown call must complete with a failure");
    let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome else {
        panic!("post-shutdown call must be a structured failure: {output:?}");
    };
    assert_eq!(failure.class, ToolFailureClass::Unavailable);
    assert_eq!(failure.code, "mcp_pool_shut_down");
    assert_eq!(failure.suggested_delay_ms, None);

    let result = pool
        .call_tool_by_id(
            &ToolId::from("mcp:4:down/8:anything"),
            &json!({}),
            &lash_core::testing::mock_attempt_context(),
        )
        .await;
    let output = result
        .as_done_output()
        .expect("post-shutdown by-id call must complete with a failure");
    let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome else {
        panic!("post-shutdown by-id call must be a structured failure: {output:?}");
    };
    assert_eq!(failure.class, ToolFailureClass::Unavailable);
    assert_eq!(failure.code, "mcp_pool_shut_down");
    assert_eq!(failure.suggested_delay_ms, None);
}

struct NativeAndMcpProvider {
    native: ToolDefinition,
    mcp: crate::McpToolProvider,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for NativeAndMcpProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        let mut manifests = vec![self.native.manifest()];
        manifests.extend(self.mcp.tool_manifests());
        manifests
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        if name == self.native.name() {
            return Some(Arc::new(self.native.contract()));
        }
        self.mcp.resolve_contract(name)
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() == self.native.name() {
            return ToolOutcome::ok(json!("native-ok")).into();
        }
        self.mcp.execute(call).await
    }
}

#[cfg(unix)]
#[tokio::test]
async fn colliding_attach_cannot_kill_native_tools_during_catalog_rebuild() {
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "catalog", "version": "1.0.0" }
        }
    });
    let list = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "tools": [{
                "name": "lookup",
                "inputSchema": { "type": "object" }
            }]
        }
    });
    let config = || McpServerConfig {
        startup_timeout_ms: 2_000,
        call_policy: McpCallPolicy::default(),
        shutdown_policy: Default::default(),
        transport: McpTransport::Stdio(McpStdioTransport {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "read -r _; printf '%s\\n' \"$INITIALIZE\"; read -r _; \
             read -r _; printf '%s\\n' \"$LIST\"; cat >/dev/null"
                    .to_string(),
            ],
            env: BTreeMap::from([
                ("INITIALIZE".to_string(), initialize.to_string()),
                ("LIST".to_string(), list.to_string()),
            ]),
            cwd: None,
        }),
    };
    let pool = McpConnectionPool::connect(BTreeMap::from([("Docs".to_string(), config())]))
        .await
        .expect("connect the original MCP server");

    let attach_result = pool.attach("docs".to_string(), config()).await;
    let native = ToolDefinition::raw(
        "tool:native/status",
        "native_status",
        "native status",
        ToolDefinition::default_input_schema(),
        json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120));
    let native_id = native.manifest.id.clone();
    let rebuilt = lash_core::ToolRegistry::from_tool_provider(Arc::new(NativeAndMcpProvider {
        native,
        mcp: crate::McpToolProvider::new(Arc::clone(&pool)),
    }));

    // With the reservation reverted, the rejected attach above becomes a second child.
    pool.shutdown_all().await;

    let registry =
        rebuilt.expect("a rejected collision must not kill the mixed native/MCP catalog");
    let manifests = registry.tool_manifests();
    assert_eq!(manifests.len(), 2, "native and original MCP tools survive");
    assert!(
        manifests
            .iter()
            .any(|manifest| manifest.name == "native_status"),
        "the native tool remains in the rebuilt catalog"
    );
    assert!(
        manifests
            .iter()
            .any(|manifest| manifest.name == mcp_name("Docs", "lookup")),
        "the original MCP tool remains in the rebuilt catalog"
    );
    let native_result = execute_by_id(
        &registry,
        &native_id,
        &json!({}),
        &lash_core::testing::mock_attempt_context(),
    )
    .await;
    assert_eq!(native_result.value_for_projection(), json!("native-ok"));

    let error = attach_result.expect_err("the colliding runtime attach must be rejected");
    assert!(matches!(error, McpError::Config(_)), "{error:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn eager_connects_start_in_parallel() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let alpha_marker = scratch.path().join("alpha.started");
    let bravo_marker = scratch.path().join("bravo.started");
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "parallel", "version": "1.0.0" }
        }
    });
    let list = json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": [] } });
    let config = |own: &std::path::Path, other: &std::path::Path| McpServerConfig {
        startup_timeout_ms: 1_000,
        call_policy: McpCallPolicy::default(),
        shutdown_policy: Default::default(),
        transport: McpTransport::Stdio(McpStdioTransport {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                ": > \"$OWN\"; while [ ! -e \"$OTHER\" ]; do sleep 0.01; done; \
             read -r _; printf '%s\\n' \"$INITIALIZE\"; read -r _; \
             read -r _; printf '%s\\n' \"$LIST\"; cat >/dev/null"
                    .to_string(),
            ],
            env: BTreeMap::from([
                ("OWN".to_string(), own.display().to_string()),
                ("OTHER".to_string(), other.display().to_string()),
                ("INITIALIZE".to_string(), initialize.to_string()),
                ("LIST".to_string(), list.to_string()),
            ]),
            cwd: None,
        }),
    };
    let pool = McpConnectionPool::connect(BTreeMap::from([
        ("alpha".to_string(), config(&alpha_marker, &bravo_marker)),
        ("bravo".to_string(), config(&bravo_marker, &alpha_marker)),
    ]))
    .await
    .expect("parallel eager connects complete");

    assert!(
        pool.server_statuses()
            .iter()
            .all(|status| status.health.is_connected()),
        "both handshakes require their peer child to have started"
    );
    pool.shutdown_all().await;
}

#[cfg(unix)]
#[tokio::test]
async fn mcp_law_catalog_miss_is_an_invalid_request() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let refresh_marker = scratch.path().join("refresh");
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2025-11-25",
            "capabilities": { "tools": { "listChanged": true } },
            "serverInfo": { "name": "directory", "version": "1.0.0" }
        }
    });
    let first_list = json!({
        "jsonrpc": "2.0", "id": 1,
        "result": { "tools": [
            { "name": "get-user", "inputSchema": { "type": "object" } },
            { "name": "get_user", "inputSchema": { "type": "object" } }
        ] }
    });
    let refreshed_list = json!({
        "jsonrpc": "2.0", "id": 2,
        "result": { "tools": [
            { "name": "get_user", "inputSchema": { "type": "object" } }
        ] }
    });
    let notification = json!({
        "jsonrpc": "2.0", "method": "notifications/tools/list_changed"
    });
    let survivor_call = json!({
        "jsonrpc": "2.0", "id": 3,
        "result": { "content": [{ "type": "text", "text": "underscore" }] }
    });
    let script = "\
        read -r _; printf '%s\\n' \"$INITIALIZE\"; \
        read -r _; read -r _; printf '%s\\n' \"$FIRST_LIST\"; \
        while [ ! -e \"$REFRESH_MARKER\" ]; do sleep 0.01; done; \
        printf '%s\\n' \"$NOTIFICATION\"; \
        read -r _; printf '%s\\n' \"$REFRESHED_LIST\"; \
        read -r _; printf '%s\\n' \"$SURVIVOR_CALL\"; cat >/dev/null";
    let pool = McpConnectionPool::connect(BTreeMap::from([(
        "directory".to_string(),
        McpServerConfig {
            startup_timeout_ms: 2_000,
            call_policy: McpCallPolicy::default(),
            shutdown_policy: Default::default(),
            transport: McpTransport::Stdio(McpStdioTransport {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), script.to_string()],
                env: BTreeMap::from([
                    ("INITIALIZE".to_string(), initialize.to_string()),
                    ("FIRST_LIST".to_string(), first_list.to_string()),
                    ("REFRESHED_LIST".to_string(), refreshed_list.to_string()),
                    (
                        "REFRESH_MARKER".to_string(),
                        refresh_marker.display().to_string(),
                    ),
                    ("NOTIFICATION".to_string(), notification.to_string()),
                    ("SURVIVOR_CALL".to_string(), survivor_call.to_string()),
                ]),
                cwd: None,
            }),
        },
    )]))
    .await
    .expect("connect list-changing collision server");

    let initial = pool.advertised_tools();
    let dropped_manifest = initial
        .iter()
        .find(|definition| definition.manifest.id.as_str() == "mcp:9:directory/8:get-user")
        .expect("hyphenated tool has its identity-derived name")
        .manifest
        .clone();
    let dropped_id = dropped_manifest.id.clone();
    let survivor_id = initial
        .iter()
        .find(|definition| definition.manifest.id.as_str() == "mcp:9:directory/8:get_user")
        .expect("underscore tool has its distinct identity-derived name")
        .manifest
        .id
        .clone();
    let resident = crate::McpToolProvider::new(Arc::clone(&pool));
    let survivor_manifest = resident
        .resolve_manifest_by_id(&survivor_id)
        .expect("survivor");
    let survivor_fixture = crate::plugin::prepared_test_call(&resident, &survivor_manifest)
        .await
        .execution_binding(json!({"kind":"mcp", "server":"directory", "tool_id":survivor_id}));
    let dropped_fixture = crate::plugin::prepared_test_call(&resident, &dropped_manifest)
        .await
        .execution_binding(json!({"kind":"mcp", "server":"directory", "tool_id":dropped_id}));
    std::fs::write(&refresh_marker, "refresh").expect("release tools/list_changed notification");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if pool.advertised_tools().len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("tools/list_changed drops the hyphenated tool");

    let deferred = crate::McpDeferredToolProvider::new(Arc::clone(&pool));
    let survivor_context = survivor_fixture.attempt("catalog-survivor");
    let survivor = execute_by_id(&deferred, &survivor_id, &json!({}), &survivor_context).await;
    assert!(
        survivor.is_success(),
        "survivor grant must remain valid: {survivor:?}"
    );
    assert_eq!(
        survivor.value_for_projection(),
        json!({"content":[{"type":"text","text":"underscore"}]})
    );

    let dropped_context = dropped_fixture.attempt("catalog-dropped");
    let dropped =
        execute_with_manifest(&deferred, &dropped_manifest, &json!({}), &dropped_context).await;
    assert!(!dropped.is_success(), "dropped tool id must be rejected");
    let lash_core::ToolCallOutcome::Failure(failure) = &dropped.as_output().outcome else {
        panic!("dropped tool must fail through the typed unknown-id path: {dropped:?}");
    };
    assert_eq!(failure.class, lash_core::ToolFailureClass::InvalidRequest);
    assert_eq!(failure.code, "mcp_unknown_tool_id");
    assert_eq!(failure.suggested_delay_ms, None);
    assert!(failure.message.contains("Unknown MCP tool id"));

    pool.shutdown_all().await;
}

#[cfg(unix)]
async fn exercise_deferred_call_across_catalog_refresh(retain_original: bool) {
    let scratch = tempfile::tempdir().expect("tempdir");
    let refresh_marker = scratch.path().join("refresh");
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2025-11-25",
            "capabilities": { "tools": { "listChanged": true } },
            "serverInfo": { "name": "directory", "version": "1.0.0" }
        }
    });
    let first_list = json!({
        "jsonrpc": "2.0", "id": 1,
        "result": { "tools": [
            { "name": "get_user", "inputSchema": { "type": "object" } }
        ] }
    });
    let refreshed_tools = if retain_original {
        vec![
            json!({ "name": "get-user", "inputSchema": { "type": "object" } }),
            json!({ "name": "get_user", "inputSchema": { "type": "object" } }),
        ]
    } else {
        vec![json!({ "name": "get-user", "inputSchema": { "type": "object" } })]
    };
    let refreshed_list = json!({
        "jsonrpc": "2.0", "id": 2,
        "result": { "tools": refreshed_tools }
    });
    let notification = json!({
        "jsonrpc": "2.0", "method": "notifications/tools/list_changed"
    });
    let hyphen_call = json!({
        "jsonrpc": "2.0", "id": 3,
        "result": { "content": [{ "type": "text", "text": "hyphen" }] }
    });
    let underscore_call = json!({
        "jsonrpc": "2.0", "id": 3,
        "result": { "content": [{ "type": "text", "text": "underscore" }] }
    });
    let script = "\
        read -r _; printf '%s\\n' \"$INITIALIZE\"; \
        read -r _; read -r _; printf '%s\\n' \"$FIRST_LIST\"; \
        while [ ! -e \"$REFRESH_MARKER\" ]; do sleep 0.01; done; \
        printf '%s\\n' \"$NOTIFICATION\"; \
        read -r _; printf '%s\\n' \"$REFRESHED_LIST\"; \
        read -r call; \
        case \"$call\" in *'\"name\":\"get_user\"'*) printf '%s\\n' \"$UNDERSCORE_CALL\";; \
        *) printf '%s\\n' \"$HYPHEN_CALL\";; esac; cat >/dev/null";
    let pool = McpConnectionPool::connect(BTreeMap::from([(
        "directory".to_string(),
        McpServerConfig {
            startup_timeout_ms: 2_000,
            call_policy: McpCallPolicy::default(),
            shutdown_policy: Default::default(),
            transport: McpTransport::Stdio(McpStdioTransport {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), script.to_string()],
                env: BTreeMap::from([
                    ("INITIALIZE".to_string(), initialize.to_string()),
                    ("FIRST_LIST".to_string(), first_list.to_string()),
                    ("REFRESHED_LIST".to_string(), refreshed_list.to_string()),
                    (
                        "REFRESH_MARKER".to_string(),
                        refresh_marker.display().to_string(),
                    ),
                    ("NOTIFICATION".to_string(), notification.to_string()),
                    ("HYPHEN_CALL".to_string(), hyphen_call.to_string()),
                    ("UNDERSCORE_CALL".to_string(), underscore_call.to_string()),
                ]),
                cwd: None,
            }),
        },
    )]))
    .await
    .expect("connect list-changing collision server");

    let initial = pool.advertised_tools();
    assert_eq!(initial.len(), 1);
    let stable_name = mcp_name("directory", "get_user");
    assert_eq!(initial[0].name(), "mcp__directory__get_user");
    assert_eq!(initial[0].name(), stable_name);
    let saved_id = initial[0].manifest.id.clone();
    let resolved = Arc::new(policy_tests::ActorPauseHook::default());
    pool.set_resolved_target_hook(Some(Arc::clone(&resolved)));
    let resident = crate::McpToolProvider::new(Arc::clone(&pool));
    let saved_manifest = resident
        .resolve_manifest_by_id(&saved_id)
        .expect("saved tool resolves");
    let fixture = crate::plugin::prepared_test_call(&resident, &saved_manifest)
        .await
        .execution_binding(json!({"kind":"mcp", "server":"directory", "tool_id":saved_id}));
    let call_pool = Arc::clone(&pool);
    let call_id = saved_id.clone();
    let call = tokio::spawn(async move {
        let deferred = crate::McpDeferredToolProvider::new(call_pool);
        let context = fixture.attempt("deferred-across-refresh");
        execute_by_id(&deferred, &call_id, &json!({}), &context).await
    });
    resolved.reached.notified().await;
    std::fs::write(&refresh_marker, "refresh").expect("release tools/list_changed notification");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let refreshed = pool.advertised_tools();
            if refreshed.len() == usize::from(retain_original) + 1
                && refreshed.iter().any(|definition| {
                    definition.manifest.id.as_str() == "mcp:9:directory/8:get-user"
                })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("tools/list_changed installs the refreshed catalog");
    let refreshed = pool.advertised_tools();
    let refreshed_original = refreshed
        .iter()
        .find(|definition| definition.manifest.id == saved_id);
    if retain_original {
        let renamed = refreshed_original.expect("surviving raw tool stays imported");
        assert_ne!(
            renamed.name(),
            stable_name,
            "collision renames both members"
        );
        assert!(renamed.name().starts_with("mcp__directory__get_user__"));
        assert!(
            refreshed
                .iter()
                .all(|definition| definition.name() != stable_name)
        );
    } else {
        assert!(refreshed_original.is_none());
        assert_eq!(
            refreshed[0].name(),
            stable_name,
            "the new raw tool inherits the bare path"
        );
        assert_ne!(refreshed[0].manifest.id, saved_id);
    }

    resolved.release.notify_one();
    let result = call.await.expect("deferred call task");
    assert!(
        result.is_success(),
        "saved deferred grant must validate: {result:?}"
    );
    assert_eq!(
        result.value_for_projection(),
        json!({"content":[{"type":"text","text":"underscore"}]}),
        "accepted call must dispatch the captured native tool after refresh"
    );
    pool.shutdown_all().await;
}

#[cfg(unix)]
#[tokio::test]
async fn deferred_call_keeps_captured_raw_target_across_catalog_refresh() {
    exercise_deferred_call_across_catalog_refresh(true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn deferred_call_uses_captured_raw_target_after_refresh_removes_original() {
    exercise_deferred_call_across_catalog_refresh(false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn attach_reaps_the_previous_child_before_starting_its_replacement() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let old_pid = scratch.path().join("old.pid");
    let overlap = scratch.path().join("overlap");
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "replace", "version": "1.0.0" }
        }
    });
    let list = json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": [] } });
    let handshake = "read -r _; printf '%s\\n' \"$INITIALIZE\"; \
                     read -r _; read -r _; printf '%s\\n' \"$LIST\"; cat >/dev/null";
    let config = |script: String, env: BTreeMap<String, String>| McpServerConfig {
        startup_timeout_ms: 2_000,
        call_policy: McpCallPolicy::default(),
        shutdown_policy: Default::default(),
        transport: McpTransport::Stdio(McpStdioTransport {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), script],
            env,
            cwd: None,
        }),
    };
    let common_env = || {
        BTreeMap::from([
            ("INITIALIZE".to_string(), initialize.to_string()),
            ("LIST".to_string(), list.to_string()),
            ("OLD_PID".to_string(), old_pid.display().to_string()),
            ("OVERLAP".to_string(), overlap.display().to_string()),
        ])
    };
    let pool = McpConnectionPool::connect(BTreeMap::from([(
        "replace".to_string(),
        config(
            format!("printf '%s\\n' \"$$\" > \"$OLD_PID\"; {handshake}"),
            common_env(),
        ),
    )]))
    .await
    .expect("connect original child");
    assert!(old_pid.exists(), "original child records its pid");

    pool.attach(
        "replace".to_string(),
        config(
            format!(
                "if kill -0 \"$(cat \"$OLD_PID\")\" 2>/dev/null; then : > \"$OVERLAP\"; fi; {handshake}"
            ),
            common_env(),
        ),
    )
    .await
    .expect("attach replacement child");

    assert!(
        !overlap.exists(),
        "replacement must start only after the previous child is reaped"
    );
    pool.shutdown_all().await;
}

// Subscribe before the call that terminates the mock. Publication follows
// discovery and catalog installation, and the generation excludes the old peer.
// watch retains the latest publication even if the actor wins the scheduling race.
#[cfg(unix)]
fn replacement_connection(
    pool: &McpConnectionPool,
    server: &str,
) -> impl std::future::Future<Output = ()> {
    let mut service = pool.entries.read_recover()[server].service.clone();
    let generation = service
        .borrow()
        .as_ref()
        .expect("initial connection is published")
        .generation;
    async move {
        service
            .wait_for(|published| {
                published
                    .as_ref()
                    .is_some_and(|peer| peer.generation > generation)
            })
            .await
            .expect("lifecycle actor stopped before acknowledging the replacement connection");
    }
}

#[cfg(all(unix, feature = "lashlang"))]
#[tokio::test]
async fn normalization_collisions_dispatch_stably_across_respawn() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let respawn_marker = scratch.path().join("respawned");
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "collision", "version": "1.0.0" }
        }
    });
    let first_list = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "tools": [
            { "name": "get-user", "inputSchema": { "type": "object" } },
            { "name": "get_user", "inputSchema": { "type": "object" } }
        ] }
    });
    let respawn_list = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "tools": [
            { "name": "get_user", "inputSchema": { "type": "object" } },
            { "name": "get-user", "inputSchema": { "type": "object" } }
        ] }
    });
    let hyphen_call = json!({
        "jsonrpc": "2.0", "id": 2,
        "result": { "content": [{ "type": "text", "text": "hyphen" }] }
    });
    let underscore_call = json!({
        "jsonrpc": "2.0", "id": 2,
        "result": { "content": [{ "type": "text", "text": "underscore" }] }
    });
    let script = "\
        read -r _; printf '%s\\n' \"$INITIALIZE\"; \
        read -r _; \
        read -r _; \
        if [ -e \"$RESPAWN_MARKER\" ]; then printf '%s\\n' \"$RESPAWN_LIST\"; \
        else : > \"$RESPAWN_MARKER\"; printf '%s\\n' \"$FIRST_LIST\"; fi; \
        read -r first_call; \
        case \"$first_call\" in *'\"name\":\"get-user\"'*) printf '%s\\n' \"$HYPHEN_CALL\";; \
        *) printf '%s\\n' \"$UNDERSCORE_CALL\";; esac";
    let servers = BTreeMap::from([(
        "directory".to_string(),
        McpServerConfig {
            startup_timeout_ms: 10_000,
            call_policy: McpCallPolicy {
                call_timeout_ms: 2_000,
                ..Default::default()
            },
            shutdown_policy: Default::default(),
            transport: McpTransport::Stdio(McpStdioTransport {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), script.to_string()],
                env: BTreeMap::from([
                    ("INITIALIZE".to_string(), initialize.to_string()),
                    ("FIRST_LIST".to_string(), first_list.to_string()),
                    ("RESPAWN_LIST".to_string(), respawn_list.to_string()),
                    ("HYPHEN_CALL".to_string(), hyphen_call.to_string()),
                    ("UNDERSCORE_CALL".to_string(), underscore_call.to_string()),
                    (
                        "RESPAWN_MARKER".to_string(),
                        respawn_marker.display().to_string(),
                    ),
                ]),
                cwd: None,
            }),
        },
    )]);
    let pool = McpConnectionPool::connect(servers)
        .await
        .expect("connect collision server");

    async fn dispatch(pool: &McpConnectionPool, operation: &str) -> Option<lash_core::ToolOutcome> {
        let definition = pool.advertised_tools().into_iter().find(|definition| {
            lash_lashlang_runtime::ToolManifestBindingExt::tool_binding(&definition.manifest)
                .ok()
                .flatten()
                .and_then(|binding| binding.operation)
                .as_deref()
                == Some(operation)
        })?;
        Some(
            pool.call_tool(
                definition.name(),
                &json!({}),
                &lash_core::testing::mock_attempt_context(),
            )
            .await,
        )
    }

    fn expected_operation(native_tool_name: &str) -> String {
        naming::build_catalog_names("directory", &["get-user", "get_user"])[native_tool_name]
            .1
            .clone()
            .operation
            .expect("MCP tools have a Lashlang operation")
    }

    fn bound_operation(pool: &McpConnectionPool, native_tool_name: &str) -> String {
        let tool_id = naming::durable_tool_id("directory", native_tool_name);
        let definition = pool
            .advertised_tools()
            .into_iter()
            .find(|definition| definition.manifest.id.as_str() == tool_id)
            .expect("raw MCP identity is advertised");
        lash_lashlang_runtime::ToolManifestBindingExt::tool_binding(&definition.manifest)
            .expect("valid Lashlang binding")
            .expect("MCP tool has a Lashlang binding")
            .operation
            .expect("MCP tools have a Lashlang operation")
    }

    let replacement = replacement_connection(&pool, "directory");
    let hyphen_operation = expected_operation("get-user");
    let underscore_operation = expected_operation("get_user");
    assert_ne!(hyphen_operation, underscore_operation);
    assert_eq!(bound_operation(&pool, "get-user"), hyphen_operation);
    assert_eq!(bound_operation(&pool, "get_user"), underscore_operation);

    let first = dispatch(&pool, &hyphen_operation)
        .await
        .expect("hyphenated Lashlang operation is available before respawn");
    assert_eq!(
        first.value_for_projection(),
        json!({"content":[{"type":"text","text":"hyphen"}]})
    );

    replacement.await;
    assert_eq!(bound_operation(&pool, "get-user"), hyphen_operation);
    assert_eq!(bound_operation(&pool, "get_user"), underscore_operation);
    let result = dispatch(&pool, &underscore_operation)
        .await
        .expect("underscore Lashlang operation is available after respawn");
    assert!(result.is_success(), "replacement call succeeds: {result:?}");
    assert_eq!(
        result.value_for_projection(),
        json!({"content":[{"type":"text","text":"underscore"}]})
    );

    pool.shutdown_all().await;
}

/// The first eager attempt fails, then a background reconnect spawns a live
/// child and blocks in discovery. Shutdown must cancel that in-progress
/// service explicitly and wait for the reconnect loop before returning;
/// keeping `pool` alive proves this does not rely on pool drop.
#[cfg(unix)]
#[tokio::test]
async fn shutdown_all_reaps_child_from_in_progress_reconnect_before_return() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let attempt_file = scratch.path().join("attempted");
    let pid_file = scratch.path().join("mcp.pid");
    let discovery_file = scratch.path().join("discovering");
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "reconnect-race", "version": "1.0.0" }
        }
    });
    let script = "\
            if [ ! -e \"$ATTEMPT_FILE\" ]; then : > \"$ATTEMPT_FILE\"; exit 1; fi; \
            printf '%s\\n' \"$$\" > \"$PID_FILE\"; \
            read -r _; printf '%s\\n' \"$RESP1\"; \
            read -r _; \
            read -r _; : > \"$DISCOVERY_FILE\"; \
            cat >/dev/null"
        .to_string();
    let servers = BTreeMap::from([(
        "race".to_string(),
        McpServerConfig {
            startup_timeout_ms: 10_000,
            call_policy: McpCallPolicy {
                call_timeout_ms: 10_000,
                ..Default::default()
            },
            shutdown_policy: Default::default(),
            transport: McpTransport::Stdio(McpStdioTransport {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), script],
                env: BTreeMap::from([
                    (
                        "ATTEMPT_FILE".to_string(),
                        attempt_file.display().to_string(),
                    ),
                    ("PID_FILE".to_string(), pid_file.display().to_string()),
                    (
                        "DISCOVERY_FILE".to_string(),
                        discovery_file.display().to_string(),
                    ),
                    ("RESP1".to_string(), initialize.to_string()),
                ]),
                cwd: None,
            }),
        },
    )]);
    let pool = McpConnectionPool::connect(servers)
        .await
        .expect("startup outage keeps the pool alive");

    tokio::time::timeout(Duration::from_secs(10), async {
        while !discovery_file.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background reconnect reaches discovery");
    let pid = std::fs::read_to_string(&pid_file)
        .expect("read reconnect child pid")
        .trim()
        .to_string();
    assert!(
        process_exists(&pid),
        "reconnect child must be live before shutdown"
    );

    pool.shutdown_all().await;

    assert!(
        !process_exists(&pid),
        "shutdown_all must reap the in-progress reconnect child before returning"
    );
    assert!(pool.shut_down.load(Ordering::SeqCst));
}

#[cfg(unix)]
fn process_exists(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .expect("probe child process")
        .success()
}

/// A connection that dies mid-life is detected on the next call and
/// re-established by the background reconnect loop; tool definitions are
/// kept across the outage so the surface stays stable.
#[tokio::test]
async fn pool_reconnects_after_transport_death() {
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "demo", "version": "1.0.0" }
        }
    });
    let list = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "tools": [{
                "name": "ping",
                "description": "Ping",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    });
    let call = json!({ "jsonrpc": "2.0", "id": 2, "result": { "content": [{ "type": "text", "text": "pong" }] } });

    // Serve initialize, tools/list, and exactly one tools/call, then exit:
    // the transport dies after the first successful call. Every reconnect
    // runs the same script again (rmcp request ids restart per connection).
    let script = "\
            read -r _; printf '%s\\n' \"$RESP1\"; \
            read -r _; \
            read -r _; printf '%s\\n' \"$RESP2\"; \
            read -r _; printf '%s\\n' \"$RESP3\""
        .to_string();

    let mut env = BTreeMap::new();
    env.insert("RESP1".to_string(), initialize.to_string());
    env.insert("RESP2".to_string(), list.to_string());
    env.insert("RESP3".to_string(), call.to_string());

    let mut servers = BTreeMap::new();
    servers.insert(
        "flaky".to_string(),
        McpServerConfig {
            startup_timeout_ms: 10_000,
            call_policy: McpCallPolicy {
                call_timeout_ms: 2_000,
                timeout_disconnect_policy: TimeoutDisconnectPolicy::Never,
                ..Default::default()
            },
            shutdown_policy: Default::default(),
            transport: McpTransport::Stdio(McpStdioTransport {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), script],
                env,
                cwd: None,
            }),
        },
    );

    let pool = McpConnectionPool::connect(servers)
        .await
        .expect("connects to the mock");
    let ctx = lash_core::testing::mock_attempt_context();
    let args = json!({});

    let reconnect = Arc::new(policy_tests::ActorPauseHook::default());
    pool.entries.read_recover()["flaky"].set_mid_establish_hook(Some(Arc::clone(&reconnect)));
    let replacement = replacement_connection(&pool, "flaky");
    let ping_name = mcp_name("flaky", "ping");
    let first = pool.call_tool(&ping_name, &args, &ctx).await;
    assert!(first.is_success(), "first call succeeds: {first:?}");

    // Pause the reconnect after the old peer is unpublished, so the outage
    // assertions run at a known state instead of depending on a scheduling race.
    reconnect.reached.notified().await;
    assert_eq!(
        pool.advertised_tools().len(),
        1,
        "tool definitions are kept across a disconnect"
    );
    let unavailable = pool.call_tool(&ping_name, &args, &ctx).await;
    let output = unavailable
        .as_done_output()
        .expect("a disconnected MCP call must complete with a failure");
    let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome else {
        panic!("a disconnected MCP call must be a structured failure: {output:?}");
    };
    assert_eq!(failure.class, ToolFailureClass::Unavailable);
    assert_eq!(failure.code, "mcp_server_unavailable");
    assert_eq!(failure.suggested_delay_ms, Some(500));
    reconnect.release.notify_one();
    replacement.await;
    assert_eq!(pool.advertised_tools().len(), 1);
    let result = pool.call_tool(&ping_name, &args, &ctx).await;
    assert!(result.is_success(), "replacement call succeeds: {result:?}");

    pool.shutdown_all().await;
}

/// Regression for the missing discovery timeout: a server that completes
/// the handshake but then hangs on `tools/list` must surface a
/// `StartupTimeout` rather than blocking `connect` forever.
#[tokio::test]
async fn discovery_hang_surfaces_startup_timeout() {
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "demo", "version": "1.0.0" }
        }
    });

    // Respond to `initialize`, swallow `notifications/initialized`, read the
    // `tools/list` request line, then hang (never respond) by blocking on
    // stdin. The short startup timeout must trip.
    let script = "\
            read -r _; printf '%s\\n' \"$RESP1\"; \
            read -r _; \
            read -r _; \
            cat >/dev/null"
        .to_string();

    let mut env = BTreeMap::new();
    env.insert("RESP1".to_string(), initialize.to_string());

    let config = McpServerConfig {
        startup_timeout_ms: 750,
        call_policy: McpCallPolicy {
            call_timeout_ms: 10_000,
            ..Default::default()
        },
        shutdown_policy: Default::default(),
        transport: McpTransport::Stdio(McpStdioTransport {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), script],
            env,
            cwd: None,
        }),
    };

    let entry = McpEntry::new("hangs".to_string(), config, McpHostServices::default());
    match entry.establish().await {
        Err(McpError::StartupTimeout { .. }) => {}
        Err(other) => panic!("expected StartupTimeout from a hung tools/list, got {other:?}"),
        Ok(_) => panic!("a hung tools/list must not connect"),
    }
    assert!(entry.service_snapshot().is_none());
    assert!(
        matches!(
            &*entry.health.read_recover(),
            McpServerHealth::Reconnecting { last_error: Some(McpServerFault::Connection(err)) }
                if err.contains("timed out") || err.contains("timeout")
        ),
        "the failure is recorded for status reporting"
    );
}

/// Regression for accidentally serializing calls behind lifecycle state: two
/// concurrent `tools/call` requests to the same server must be able to be
/// in flight at once. The mock refuses to answer the first call until it
/// has read the second request line, so a serializing implementation (lock
/// held across `.await`) would deadlock and time out, while the concurrent
/// implementation completes both calls.
#[tokio::test]
async fn concurrent_calls_are_not_serialized_by_the_service_mutex() {
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "demo", "version": "1.0.0" }
        }
    });
    let list = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "tools": [{
                "name": "ping",
                "description": "Ping",
                "inputSchema": { "type": "object", "properties": {} }
            }]
        }
    });
    // rmcp assigns request ids 2 and 3 to the two concurrent calls. The
    // mock reads BOTH request lines before emitting EITHER response, which
    // is only possible if both requests are in flight concurrently.
    let call2 = json!({ "jsonrpc": "2.0", "id": 2, "result": { "content": [{ "type": "text", "text": "pong" }] } });
    let call3 = json!({ "jsonrpc": "2.0", "id": 3, "result": { "content": [{ "type": "text", "text": "pong" }] } });

    let script = "\
            read -r _; printf '%s\\n' \"$RESP1\"; \
            read -r _; \
            read -r _; printf '%s\\n' \"$RESP2\"; \
            read -r _; \
            read -r _; \
            printf '%s\\n' \"$RESP3\"; \
            printf '%s\\n' \"$RESP4\"; \
            cat >/dev/null"
        .to_string();

    let mut env = BTreeMap::new();
    env.insert("RESP1".to_string(), initialize.to_string());
    env.insert("RESP2".to_string(), list.to_string());
    env.insert("RESP3".to_string(), call2.to_string());
    env.insert("RESP4".to_string(), call3.to_string());

    let mut servers = BTreeMap::new();
    servers.insert(
        "svc".to_string(),
        McpServerConfig {
            startup_timeout_ms: 10_000,
            call_policy: McpCallPolicy {
                call_timeout_ms: 5_000,
                ..Default::default()
            },
            shutdown_policy: Default::default(),
            transport: McpTransport::Stdio(McpStdioTransport {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), script],
                env,
                cwd: None,
            }),
        },
    );

    let pool = McpConnectionPool::connect(servers)
        .await
        .expect("connects to concurrency mock");

    let ctx = lash_core::testing::mock_attempt_context();
    let args = json!({});
    let ping_name = mcp_name("svc", "ping");
    let (a, b) = tokio::join!(
        pool.call_tool(&ping_name, &args, &ctx),
        pool.call_tool(&ping_name, &args, &ctx),
    );
    assert!(a.is_success(), "first concurrent call failed: {a:?}");
    assert!(b.is_success(), "second concurrent call failed: {b:?}");

    pool.shutdown_all().await;
}

/// FIG-4708 P1: a stdio child that floods stdout past the inbound message cap
/// is closed by the transport and the disconnect is recorded with the typed
/// cause rather than rmcp's generic quit reason.
#[cfg(unix)]
#[tokio::test]
async fn oversized_stdio_message_disconnects_with_typed_cause() {
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "flooder", "version": "1.0.0" }
        }
    });
    let list = json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": [] } });
    // Answer initialize and tools/list, then write one newline-free message
    // larger than MAX_STDIO_MESSAGE_BYTES and stay alive so the pool — not
    // the child's own exit — does the closing.
    let script = "\
        read -r _; printf '%s\\n' \"$INITIALIZE\"; \
        read -r _; \
        read -r _; printf '%s\\n' \"$LIST\"; \
        head -c 9000000 /dev/zero | tr '\\000' 'x'; \
        cat >/dev/null";
    let pool = McpConnectionPool::connect(BTreeMap::from([(
        "flooder".to_string(),
        McpServerConfig {
            startup_timeout_ms: 2_000,
            call_policy: McpCallPolicy::default(),
            shutdown_policy: Default::default(),
            transport: McpTransport::Stdio(McpStdioTransport {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), script.to_string()],
                env: BTreeMap::from([
                    ("INITIALIZE".to_string(), initialize.to_string()),
                    ("LIST".to_string(), list.to_string()),
                ]),
                cwd: None,
            }),
        },
    )]))
    .await
    .expect("the flood starts only after the handshake completes");

    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let status = &pool.server_statuses()[0];
            if let Some(error) = status.health.error()
                && error.contains("inbound message exceeded the 8388608-byte limit")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the typed overflow cause reaches the server's fault record");
    pool.shutdown_all().await;
}

#[tokio::test]
async fn mcp_law_tool_error_preserves_cause_and_attachment_roots() {
    let result = serde_json::from_value(json!({
        "content":[
            {"type":"text","text":"bad input"},
            {"type":"image","data":"YQ==","mimeType":"image/png"}
        ],
        "structuredContent":{"$lash_tool_value":"attachment","source":{"foreign":true}},
        "isError":true
    }))
    .expect("valid MCP result");
    let root = tempfile::tempdir().expect("attachment directory");
    let store = Arc::new(
        lash_core::facade_support::RuntimeAttachmentStore::ephemeral(Arc::new(
            lash_core::facade_support::FileAttachmentStore::new(root.path()),
        )),
    );
    let controller = lash_core::ActorContext::unavailable();
    let dispatch = lash_core::testing::TestExecutionContextBuilder::over_controller(controller)
        .attachment_store(store)
        .build()
        .dispatch;
    let fixture = lash_core::testing::ToolCallFixture::from_dispatch(dispatch);
    let output = tool_result_from_rmcp(result, &fixture.attempt("test-turn"))
        .await
        .into_done_output()
        .expect("settled");
    let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome else {
        panic!("tool error must fail");
    };
    assert_eq!(failure.class, ToolFailureClass::Execution);
    assert_eq!(failure.code, "mcp_tool_error");
    assert_eq!(failure.source, ToolFailureSource::Tool);
    assert_eq!(failure.message, "bad input");
    let raw = failure.raw.as_ref().expect("typed cause");
    assert_eq!(raw.to_json_value()["kind"], "tool_error");
    let roots = raw.attachments();
    assert_eq!(roots.len(), 1, "only the retained image is a typed root");
    let reloaded: lash_core::ToolCallOutput =
        serde_json::from_value(serde_json::to_value(&output).expect("recorded output"))
            .expect("replayed output");
    let lash_core::ToolCallOutcome::Failure(failure) = reloaded.outcome else {
        panic!("replay preserves failure");
    };
    assert_eq!(failure.raw.expect("recorded cause").attachments(), roots);
}
