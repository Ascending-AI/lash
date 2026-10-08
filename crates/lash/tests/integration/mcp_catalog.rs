#![cfg(feature = "mcp")]
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash::direct::LlmOutputPart;
use lash::mcp::{McpPluginFactory, McpServerConfig, McpServerHealth, McpStdioTransport};
use lash::plugins::PluginFactory;
use lash::provider::LlmResponse;
use lash_postgres_store::{PostgresStorage, PostgresStoreSet, testing::IsolatedDatabase};

/// The session's prompt sections composed for a turn call that offers every
/// tool of `catalog` natively (ADR 0133).
#[expect(
    clippy::expect_used,
    reason = "a test helper fails loudly on a prompt that does not compose"
)]
async fn turn_prompt(
    session: &lash_core::plugin::PluginSession,
    catalog: &lash_core::ToolCatalog,
) -> String {
    use lash::plugins::{OfferedTools, PromptCall};
    use lash::prompt::{PromptPlan, PromptPurpose};
    use lash_core::testing::prompt::{PromptCutParts, compose};
    let cut = lash_core::testing::prompt::cut(PromptCutParts {
        call: PromptCall {
            session_id: lash::SessionId::from("advertised-surface"),
            frame: None,
            run: None,
            turn: None,
            iteration: 0,
            call: 0,
            purpose: PromptPurpose::Turn,
        },
        config: session.admitted_plugin_config(),
        session: None,
        offered: OfferedTools::new(Arc::new(catalog.clone()), false),
        model: Default::default(),
        history: Default::default(),
        namespaces: Default::default(),
    });
    compose(
        &session.prompt_catalog(),
        &PromptPlan::default(),
        &PromptPurpose::Turn,
        cut,
    )
    .await
    .expect("the prompt composes")
    .initial_instructions
    .unwrap_or_default()
}

/// L3 (FIG-4859): a reopened session's recorded tool surface is not rewritten
/// when the server's advertised tools changed since it was recorded — the
/// recorded catalog still serves and every missing or moved entry is judged
/// by its typed drift.
#[tokio::test]
async fn recorded_tool_surface_is_preserved_when_advertised_tools_change() {
    fn peer(server: &str, tool: &str) -> McpServerConfig {
        let initialize = serde_json::json!({
            "jsonrpc":"2.0", "id":0, "result": {
                "protocolVersion":"2025-11-25", "capabilities":{"tools":{}},
                "serverInfo":{"name":server,"version":"1"},
                "instructions": format!("the {tool}-era peer instructions")
            }
        });
        let tools = serde_json::json!({"jsonrpc":"2.0", "id":1, "result":{"tools":[
            {"name":tool, "description":format!("the {tool} tool"), "inputSchema":{"type":"object"}}
        ]}});
        McpServerConfig::stdio(McpStdioTransport::new("sh", vec![
            "-c".to_string(),
            "read -r _; printf '%s\\n' \"$INITIALIZE\"; read -r _; read -r _; printf '%s\\n' \"$TOOLS\"; cat >/dev/null".to_string(),
        ]).with_env([
            ("INITIALIZE", initialize.to_string()),
            ("TOOLS", tools.to_string()),
        ]))
    }

    let protocol = || {
        Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new())
            as Arc<dyn PluginFactory>
    };
    let lookup = lash::mcp::mcp_tool_names("records", &["lookup"])["lookup"].clone();
    let search = lash::mcp::mcp_tool_names("records", &["search"])["search"].clone();

    // The session was created while the server advertised `lookup`.
    let created_factory = Arc::new(
        McpPluginFactory::new(BTreeMap::from([(
            "records".to_string(),
            peer("created-peer", "lookup"),
        )]))
        .await
        .expect("created peer connects"),
    );
    // The plugin configuration a real creation records: the protocol's
    // recorded `behaviour` is what its rematerialization must find.
    let created_host =
        lash_core::facade_support::PluginHost::new(vec![protocol(), created_factory.clone()]);
    let plugin_config = created_host
        .resolve_creation_plugin_config(
            Some(lash_protocol_standard::STANDARD_PROTOCOL_PLUGIN_ID),
            &lash_core::PluginOptions::default(),
            &lash_core::store::plugin_writers::PluginAdmission::default(),
        )
        .expect("the creation config resolves");
    let created = created_host
        .build_session(lash_core::plugin::PluginSessionRequest::creation(
            "advertised-surface",
            lash_core::plugin::SessionAuthorityContext {
                plugin_config: lash_core::AdmittedPluginConfig::new(plugin_config, 0),
                ..Default::default()
            },
        ))
        .expect("created session");
    let recorded = created.resolved_tool_catalog().expect("recorded catalog");
    assert!(recorded.has_callable_tool(&lookup), "lookup advertised");
    let recorded_bytes = serde_json::to_vec(&*recorded).expect("record the catalog");
    let snapshot = created.export_state();
    let config = created.admitted_plugin_config();

    // The reopened deployment's server advertises `search` instead.
    let reopened_factory = Arc::new(
        McpPluginFactory::new(BTreeMap::from([(
            "records".to_string(),
            peer("reopened-peer", "search"),
        )]))
        .await
        .expect("reopened peer connects"),
    );
    let reopened =
        lash_core::facade_support::PluginHost::new(vec![protocol(), reopened_factory.clone()])
            .build_session(lash_core::plugin::PluginSessionRequest::rematerialization(
                "advertised-surface",
                &snapshot,
                lash_core::plugin::SessionAuthorityContext {
                    plugin_config: config,
                    ..Default::default()
                },
            ))
            .expect("reopened session");
    let live = reopened.resolved_tool_catalog().expect("live catalog");
    assert!(
        live.has_callable_tool(&search),
        "the live surface follows the advertisement"
    );
    assert!(!live.has_callable_tool(&lookup));

    // Serving the recorded surface consults the record, not the advertisement:
    // the recorded catalog names `lookup`, and judged against the live
    // catalog its drift is typed, not substituted.
    let restored: lash_core::ToolCatalog =
        serde_json::from_slice(&recorded_bytes).expect("restore the recorded catalog");
    assert!(restored.has_callable_tool(&lookup));
    assert!(!restored.has_callable_tool(&search));
    let recorded_definition = lash_core::ToolDefinition {
        manifest: restored
            .tools
            .iter()
            .find(|entry| entry.manifest.name == lookup)
            .expect("the recorded surface names lookup")
            .manifest
            .clone(),
        contract: (*restored
            .tools
            .iter()
            .find(|entry| entry.manifest.name == lookup)
            .expect("the recorded surface names lookup")
            .contract)
            .clone(),
    };
    let drift = lash_core::ToolSurfaceDrift::judge(&recorded_definition, &live)
        .expect("a recorded tool the advertisement dropped drifts");
    assert_eq!(drift.kind, lash_core::ToolSurfaceDriftKind::Missing);
    // The server's guidance section renders the guidance the offered
    // manifests pin: the recorded catalog carries the creating peer's
    // instructions, so rendering it serves them while the live catalog
    // serves the successor's.
    let recorded_prompt = turn_prompt(&reopened, &restored).await;
    assert!(
        recorded_prompt.contains("the lookup-era peer instructions"),
        "the record is served: {recorded_prompt}"
    );
    assert!(
        !recorded_prompt.contains("the search-era peer instructions"),
        "the advertisement does not leak in"
    );
    let live_prompt = turn_prompt(&reopened, &live).await;
    assert!(
        live_prompt.contains("the search-era peer instructions"),
        "the live surface is the advertisement's own"
    );
    assert_eq!(
        reopened.admitted_plugin_config(),
        created.admitted_plugin_config(),
        "the recorded config is preserved"
    );
    created_factory
        .shutdown()
        .await
        .expect("created peer shutdown");
    reopened_factory
        .shutdown()
        .await
        .expect("reopened peer shutdown");
}

/// A server's instructions render once per module on every prompt surface a
/// session offers its tools on, from the recorded catalog alone.
#[tokio::test]
async fn server_instructions_render_once_per_module_on_every_prompt_surface() {
    const INSTRUCTIONS: &str = "Authenticate with login before searching. Follow every nextCursor.";
    let initialize = serde_json::json!({
        "jsonrpc":"2.0", "id":0, "result": {
            "protocolVersion":"2025-11-25", "capabilities":{"tools":{}},
            "serverInfo":{"name":"instructions-peer","version":"1"},
            "instructions": INSTRUCTIONS
        }
    });
    let tools = serde_json::json!({"jsonrpc":"2.0", "id":1, "result":{"tools":[
        {"name":"login", "description":"Authenticate", "inputSchema":{"type":"object"}},
        {"name":"search", "description":"Search records", "inputSchema":{"type":"object"}}
    ]}});
    let factory = Arc::new(McpPluginFactory::new(BTreeMap::from([(
        "records".to_string(),
        McpServerConfig::stdio(McpStdioTransport::new("sh", vec![
            "-c".to_string(),
            "read -r _; printf '%s\\n' \"$INITIALIZE\"; read -r _; read -r _; printf '%s\\n' \"$TOOLS\"; cat >/dev/null".to_string()
        ]).with_env([("INITIALIZE", initialize.to_string()), ("TOOLS", tools.to_string())]))
    )])).await.expect("instruction peer connects"));
    assert_eq!(factory.pool().advertised_tools().len(), 2);
    let backend = crate::support::sqlite_memory_store_backend().await;
    let mut observations = Vec::new();
    for surface in ["standard", "cell", "native"] {
        let (protocol, protocol_id): (Arc<dyn PluginFactory>, &str) = if surface == "standard" {
            (
                Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new()),
                lash_protocol_standard::STANDARD_PROTOCOL_PLUGIN_ID,
            )
        } else {
            (
                Arc::new(
                    lash_protocol_rlm::RlmProtocolPluginFactory::new(
                        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                            .channel(if surface == "cell" {
                                lash_protocol_rlm::RlmChannel::Cell
                            } else {
                                lash_protocol_rlm::RlmChannel::NativeTool
                            })
                            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(
                                1000,
                            ))
                            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(1))
                            .build(),
                        Arc::new(lash_protocol_rlm::TypescriptDialect),
                        &backend,
                    )
                    .with_process_lifecycle(false),
                ),
                lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID,
            )
        };
        let host = lash_core::facade_support::PluginHost::new(vec![protocol, factory.clone()]);
        let plugin_config = host
            .resolve_creation_plugin_config(
                Some(protocol_id),
                &lash_core::PluginOptions::default(),
                &lash_core::store::plugin_writers::PluginAdmission::default(),
            )
            .expect("the creation config resolves");
        let session = host
            .build_session(lash_core::plugin::PluginSessionRequest::creation(
                surface,
                lash_core::plugin::SessionAuthorityContext {
                    plugin_config: lash_core::AdmittedPluginConfig::new(plugin_config, 0),
                    ..Default::default()
                },
            ))
            .expect("prompt session");
        let catalog = session.resolved_tool_catalog().expect("captured catalog");
        // The recorded catalog is sufficient to rebuild the prompt without
        // consulting the peer.
        let recorded = serde_json::to_vec(catalog.as_ref()).expect("record catalog");
        let catalog: lash_core::ToolCatalog =
            serde_json::from_slice(&recorded).expect("restore catalog");
        let prompt = turn_prompt(&session, &catalog).await;
        observations.push((surface, prompt.matches(INSTRUCTIONS).count()));
    }
    factory.shutdown().await.expect("peer shutdown");
    assert_eq!(observations, [("standard", 1), ("cell", 1), ("native", 1)]);
}

const PEER: &str = r#"
import json, os, sys, threading, time
lock = threading.Lock()
started = threading.Event()
pending = []
root = os.environ['FIXTURE_ROOT']

def send(message):
    with lock:
        sys.stdout.write(json.dumps(message, separators=(',', ':')))
        sys.stdout.flush()
        sys.stdout.write('\n')
        sys.stdout.flush()

def reply_lists():
    while not os.path.exists(root + '/release'):
        time.sleep(0.001)
    while True:
        with lock:
            replies = pending[:]
            pending.clear()
        for request_id, index in replies:
            send({'jsonrpc':'2.0', 'id':request_id, 'result':{'tools':[
                {'name':'work-' + str(index), 'inputSchema':{'type':'object'}}]}})
        time.sleep(0.001)
threading.Thread(target=reply_lists, daemon=True).start()

def storm(request_id):
    notification = json.dumps({'jsonrpc':'2.0', 'method':'notifications/tools/list_changed'}, separators=(',', ':')) + '\n'
    with lock:
        sys.stdout.write(notification * 100000)
        sys.stdout.flush()
    started.wait()
    send({'jsonrpc':'2.0', 'id':request_id, 'result':{'content':[{'type':'text','text':'storm sent'}]}})

lists = 0
for line in sys.stdin:
    message = json.loads(line)
    method = message.get('method')
    if method == 'initialize':
        send({'jsonrpc':'2.0', 'id':message['id'], 'result':{
            'protocolVersion':'2025-11-25', 'capabilities':{'tools':{'listChanged':True}},
            'serverInfo':{'name':'catalog-turn-peer','version':'1'}}})
    elif method == 'tools/list':
        lists += 1
        with open(root + '/lists', 'w') as trace:
            trace.write(str(lists))
        if lists == 1:
            send({'jsonrpc':'2.0', 'id':message['id'], 'result':{'tools':[
                {'name':'storm', 'inputSchema':{'type':'object'}}]}})
        else:
            with lock:
                pending.append((message['id'], lists))
            started.set()
    elif method == 'tools/call':
        if os.environ.get('FAILURE_LAW') == 'true':
            if message['params'].get('arguments', {}).get('attachment'):
                send({'jsonrpc':'2.0', 'id':message['id'], 'result':{'content':[
                    {'type':'image', 'data':'YWJj', 'mimeType':'image/png'}]}})
            else:
                send({'jsonrpc':'2.0', 'id':message['id'], 'error':{
                    'code':-32602, 'message':'bad field', 'data':{'field':'query'}}})
        else:
            threading.Thread(target=storm, args=(message['id'],), daemon=True).start()
    elif method == 'ping':
        send({'jsonrpc':'2.0', 'id':message['id'], 'result':{}})
"#;

#[derive(Clone, Copy)]
enum Store {
    SqliteMemory,
    SqliteFile,
    Postgres,
}

#[expect(
    clippy::expect_used,
    reason = "integration fixture setup and assertions"
)]
async fn make_stores(
    store: Store,
    root: &std::path::Path,
    storage: Option<&PostgresStorage>,
) -> Arc<dyn lash_core::StoreSet> {
    match store {
        Store::SqliteMemory => Arc::new(
            lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .expect("SQLite memory"),
        ),
        Store::SqliteFile => Arc::new(
            lash_sqlite_store::SqliteStoreSet::open(root.join("stores.db"))
                .await
                .expect("SQLite file"),
        ),
        Store::Postgres => Arc::new(PostgresStoreSet::new(
            storage.expect("PostgreSQL storage"),
            lash_sqlite_store::SqliteStoreSet::open(
                (root.join("attachments")).join("attachments.db"),
            )
            .await
            .expect("SQLite attachment store")
            .attachment_store(),
        )),
    }
}

/// One turn on the durable engine over `store` whose model calls the MCP
/// peer's `storm` tool. The storm floods `tools/list_changed` while the
/// discovery it triggers stalls, and the turn still completes. Under the
/// `failure_law`, the peer answers a JSON-RPC error and an attachment past
/// the size limit, each recorded as its typed failure.
#[expect(
    clippy::expect_used,
    reason = "integration fixture setup and assertions"
)]
async fn turn_witness(store: Store, failure_law: bool) {
    let root = tempfile::tempdir().expect("fixture directory");
    let database = if matches!(store, Store::Postgres) {
        let url = std::env::var("LASH_POSTGRES_DATABASE_URL").expect("managed PostgreSQL gate URL");
        Some(IsolatedDatabase::create(&url).await)
    } else {
        None
    };
    let storage = if let Some(database) = &database {
        Some(
            lash_postgres_store::testing::connect(database.url())
                .await
                .expect("PostgreSQL storage"),
        )
    } else {
        None
    };
    let backend =
        lash_conformance::backend_over(make_stores(store, root.path(), storage.as_ref()).await);
    let factory = Arc::new(
        McpPluginFactory::new(BTreeMap::from([(
            "catalog".to_string(),
            McpServerConfig::stdio(
                McpStdioTransport::new(
                    "python3",
                    vec!["-u".to_string(), "-c".to_string(), PEER.to_string()],
                )
                .with_env([
                    ("FIXTURE_ROOT", root.path().display().to_string()),
                    ("FAILURE_LAW", failure_law.to_string()),
                ]),
            )
            .with_timeouts(
                Duration::from_secs(600),
                Duration::from_secs(600),
                Duration::from_secs(900),
            ),
        )]))
        .await
        .expect("MCP peer"),
    );
    assert!(
        matches!(
            factory.server_statuses()[0].health,
            McpServerHealth::Connected { .. }
        ),
        "{:?}",
        factory.server_statuses()
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let name = lash::mcp::mcp_tool_names("catalog", &["storm"])["storm"].clone();
    let provider = lash_core::testing::TestProvider::builder()
        .complete(move |_| {
            let call = observed.fetch_add(1, Ordering::SeqCst);
            let extra = if failure_law && call == 0 {
                Some(LlmOutputPart::ToolCall {
                    call_id: "attachment-1".to_string(),
                    tool_name: name.clone(),
                    input_json: "{\"attachment\":true}".to_string(),
                    replay: None,
                })
            } else {
                None
            };
            let part = match call {
                0 => LlmOutputPart::ToolCall {
                    call_id: "storm-1".to_string(),
                    tool_name: name.clone(),
                    input_json: "{}".to_string(),
                    replay: None,
                },
                1 => LlmOutputPart::Text {
                    text: "storm done".to_string(),
                    response_meta: None,
                },
                other => panic!("unexpected model call {other}"),
            };
            async move {
                Ok(LlmResponse {
                    parts: std::iter::once(part).chain(extra).collect(),
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    let core = lash::LashCore::standard_builder(backend)
        .llm_profiles(std::sync::Arc::new(
            lash::LlmProfileRegistry::new()
                .register(
                    "catalog-fixture",
                    lash::RegisteredLlmProfile::new(
                        lash::LlmProfileMetadata::builder("catalog-fixture")
                            .context_window_tokens(16_000)
                            .build()
                            .expect("model"),
                        provider,
                    ),
                )
                .expect("register the test model"),
        ))
        .max_attachment_bytes(failure_law.then_some(1))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .plugin(factory.clone())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "mcp-catalog",
            uuid::Uuid::new_v4().to_string(),
        ))
        .expect("core");
    let session = crate::created_session(&core, "catalog-fixture", "catalog-storm")
        .await
        .durable()
        .await
        .expect("session");
    let output = tokio::time::timeout(
        Duration::from_secs(600),
        session
            .send(lash::TurnInput::text("Run the catalog storm"))
            .output(),
    )
    .await
    .expect("turn deadline")
    .expect("turn completes during stalled discovery");
    assert_eq!(
        output.result.tool_calls.len(),
        if failure_law { 2 } else { 1 },
        "{:?}",
        output.result.tool_calls
    );
    if failure_law {
        let persisted = session
            .read()
            .await
            .expect("durable read")
            .expect("persisted session");
        assert_eq!(persisted.turn_index(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        factory.shutdown().await.expect("MCP shutdown");
        for (record, class, code, kind) in output
            .result
            .tool_calls
            .iter()
            .zip([
                (
                    lash::tools::ToolFailureClass::InvalidRequest,
                    "mcp_json_rpc_error",
                    "json_rpc",
                ),
                (
                    lash::tools::ToolFailureClass::ResourceLimit,
                    "mcp_attachment_store",
                    "attachment_store",
                ),
            ])
            .rev()
            .map(|(record, (class, code, kind))| (record, class, code, kind))
        {
            let lash::tools::ToolCallOutcome::Failure(error) = &record.output.outcome else {
                panic!("expected typed failure: {record:?}");
            };
            assert_eq!(error.class, class);
            assert_eq!(error.code, code);
            assert_eq!(error.suggested_delay_ms, None);
            let raw = error.raw.as_ref().expect("typed cause").to_json_value();
            assert_eq!(raw["kind"], kind);
            if kind == "json_rpc" {
                assert_eq!(raw["error"]["code"], -32602);
                assert_eq!(raw["error"]["data"], serde_json::json!({"field":"query"}));
            } else {
                assert_eq!(
                    raw["cause"],
                    serde_json::json!({"kind":"size_limit_exceeded", "byte_len":3, "max_bytes":1})
                );
            }
            let replayed: lash::tools::ToolCallOutput = serde_json::from_slice(
                &serde_json::to_vec(&record.output).expect("recorded output"),
            )
            .expect("replayed output");
            assert_eq!(replayed.outcome, record.output.outcome);
        }
        drop(session);
        core.shutdown().await.expect("the core shuts down");
        drop(database);
        return;
    }
    assert_eq!(output.result.assistant_message(), Some("storm done"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        std::fs::read_to_string(root.path().join("lists")).expect("discovery trace"),
        "2",
        "storm keeps exactly one stalled discovery"
    );
    std::fs::write(root.path().join("release"), "release").expect("release catalog");
    tokio::time::timeout(Duration::from_secs(600), async {
        while !factory
            .pool()
            .advertised_tools()
            .iter()
            .any(|tool| tool.name().contains("__work_"))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("valid refresh publication");
    let reopened = core
        .session(lash::SessionId::parse("catalog-storm").expect("nonblank host identity"))
        .open()
        .await
        .expect("reopen committed session");
    assert!(reopened.read_view().chronological_projection().into_entries().iter().any(|entry| {
        matches!(&entry.payload, lash::persistence::ChronologicalPayload::Message(message) if message.parts.iter().any(|part| part.content().contains("storm done")))
    }), "the completed turn survives a store reload");
    factory.shutdown().await.expect("MCP shutdown");
    drop(session);
    drop(reopened);
    core.shutdown().await.expect("the core shuts down");
    drop(database);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_storm_sqlite_memory_turn_witness() {
    turn_witness(Store::SqliteMemory, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_storm_sqlite_file_turn_witness() {
    turn_witness(Store::SqliteFile, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn catalog_storm_postgres_turn_witness() {
    turn_witness(Store::Postgres, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_law_turn_failures_sqlite_memory() {
    turn_witness(Store::SqliteMemory, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_law_turn_failures_sqlite_file() {
    turn_witness(Store::SqliteFile, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn mcp_law_turn_failures_postgres() {
    turn_witness(Store::Postgres, true).await;
}

/// D-DEFAULTS2 / FIG-5494: optional MCP lifecycle choices survive host JSON.
#[test]
fn mcp_lifecycle_choices_are_preserved_in_host_json() {
    for reconnect in [
        serde_json::json!("disabled"),
        serde_json::json!({"finite": 2}),
        serde_json::json!("unlimited"),
    ] {
        let authored = serde_json::json!({
            "transport": "stdio", "command": "srv",
            "startup_timeout_ms": 41,
            "call_timeout_ms": 17, "call_max_total_timeout_ms": 53,
            "reset_call_timeout_on_progress": false,
            "timeout_disconnect_policy": "consecutive_timeouts",
            "liveness_probe_timeout_ms": 7,
            "consecutive_timeouts_before_disconnect": 2,
            "liveness_probe_interval_ms": 11,
            "reconnect_initial_backoff_ms": 3, "reconnect_max_backoff_ms": 13,
            "reconnect_max_attempts": reconnect,
            "graceful_period_ms": 19, "post_kill_wait_ms": 23,
            "scheduling_margin_ms": 29, "child_exit_poll_interval_ms": 2
        });
        let config: McpServerConfig = serde_json::from_value(authored.clone())
            .expect("explicit reconnect modes are admitted");
        assert_eq!(serde_json::to_value(config).expect("host JSON"), authored);
    }
    for reconnect in [serde_json::json!(0), serde_json::json!({"finite": 0})] {
        assert!(
            serde_json::from_value::<McpServerConfig>(serde_json::json!({
                "transport":"stdio", "command":"srv", "reconnect_max_attempts":reconnect
            }))
            .is_err(),
            "overloaded zero is not a 1.0 reconnect choice"
        );
    }
    let omitted: McpServerConfig = serde_json::from_value(serde_json::json!({
        "transport":"stdio", "command":"srv"
    }))
    .expect("lifecycle choices stay optional");
    assert_eq!(
        omitted,
        McpServerConfig::stdio(McpStdioTransport::new("srv", vec![]))
    );
}

const LIFECYCLE_PEER: &str = r#"
import json, os, signal, sys, threading, time
if os.environ['MODE'] == 'shutdown':
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
with open(os.environ['STARTS'], 'a') as f:
    f.write('start\n')
def send(m):
    print(json.dumps(m), flush=True)
for line in sys.stdin:
    m = json.loads(line)
    method = m.get('method')
    if method == 'initialize' and os.environ['MODE'] != 'startup':
        send({'jsonrpc':'2.0','id':m['id'],'result':{
            'protocolVersion':'2025-11-25','capabilities':{'tools':{}},
            'serverInfo':{'name':'lifecycle','version':'1'}}})
    elif method == 'tools/list':
        send({'jsonrpc':'2.0','id':m['id'],'result':{'tools':[
            {'name':'work','inputSchema':{'type':'object'}}]}})
    elif method == 'tools/call' and os.environ['MODE'] == 'progress':
        token = m['params']['_meta']['progressToken']
        def progress(token):
            for i in range(100):
                send({'jsonrpc':'2.0','method':'notifications/progress',
                    'params':{'progressToken':token,'progress':i}})
                time.sleep(0.005)
        threading.Thread(target=progress, args=(token,), daemon=True).start()
if os.environ['MODE'] == 'shutdown':
    while True:
        time.sleep(1)

"#;

/// D-DEFAULTS2: facade call/probe choices govern actual attempts and bindings.
#[tokio::test]
async fn mcp_call_policy_controls_attempts_and_recorded_bindings() {
    use lash::mcp::{
        McpCallPolicy, McpConnectionPool, McpShutdownPolicy, ReconnectAttempts,
        TimeoutDisconnectPolicy,
    };
    for (mode, disconnect) in [
        ("silent", TimeoutDisconnectPolicy::Never),
        ("progress", TimeoutDisconnectPolicy::Never),
        ("silent", TimeoutDisconnectPolicy::PingProbe),
    ] {
        let root = tempfile::tempdir().expect("fixture");
        let policy = McpCallPolicy {
            call_timeout_ms: 20,
            call_max_total_timeout_ms: 70,
            timeout_disconnect_policy: disconnect,
            liveness_probe_timeout_ms: 30,
            reconnect_max_attempts: ReconnectAttempts::Disabled,
            ..McpCallPolicy::standard()
        };
        let mut config = McpServerConfig::stdio(
            McpStdioTransport::new(
                "python3",
                vec!["-u".into(), "-c".into(), LIFECYCLE_PEER.into()],
            )
            .with_env([
                ("MODE", mode.to_string()),
                ("STARTS", root.path().join("starts").display().to_string()),
            ]),
        );
        config.call_policy = policy.clone();
        config.shutdown_policy = McpShutdownPolicy {
            graceful_period: Duration::from_millis(5),
            post_kill_wait: Duration::from_millis(7),
            scheduling_margin: Duration::from_millis(11),
            child_exit_poll_interval: Duration::from_millis(2),
        };
        assert_eq!(
            config.shutdown_policy.total_bound(),
            Duration::from_millis(30)
        );
        let pool = McpConnectionPool::connect(BTreeMap::from([("policy".into(), config)]))
            .await
            .expect("connect");
        let tools = pool.advertised_tools();
        let tool = tools.first().expect("imported tool");
        let binding = &tool.manifest.bindings["lash.mcp"];
        assert_eq!(
            binding["call_policy"],
            serde_json::to_value(policy).expect("resolved policy")
        );
        let restored: lash::tools::ToolManifest =
            serde_json::from_slice(&serde_json::to_vec(&tool.manifest).expect("record binding"))
                .expect("cold decode of recorded manifest");
        assert_eq!(restored.bindings["lash.mcp"], *binding);
        let result = pool
            .call_tool(
                tool.name(),
                &serde_json::json!({}),
                &lash::testing::mock_attempt_context(),
            )
            .await;
        let output = result.as_done_output().expect("inline attempt");
        let lash::tools::ToolCallOutcome::Failure(failure) = &output.outcome else {
            panic!("silent tool must time out: {output:?}");
        };
        let raw = failure.raw.as_ref().expect("typed timeout").to_json_value();
        if disconnect == TimeoutDisconnectPolicy::PingProbe {
            assert_eq!(
                raw["cause"],
                serde_json::json!({"kind":"timeout", "timeout_ms":30})
            );
            assert_eq!(failure.suggested_delay_ms, None);
            assert!(failure.message.contains("automatic reconnect is disabled"));
        } else {
            assert_eq!(raw["timeout_ms"], if mode == "progress" { 70 } else { 20 });
            assert_eq!(raw["deadline"], mode == "progress");
        }
        pool.shutdown_all().await;
    }
}

/// FIG-5494: disabled reconnect remains disabled across keepalive ticks.
/// D-DEFAULTS2: a non-default facade startup deadline reaches the handshake.
#[tokio::test]
async fn mcp_disabled_reconnect_survives_startup_timeout_and_keepalive() {
    use lash::mcp::{McpCallPolicy, McpConnectionPool, McpShutdownPolicy, ReconnectAttempts};
    let root = tempfile::tempdir().expect("fixture");
    let mut config = McpServerConfig::stdio(
        McpStdioTransport::new(
            "python3",
            vec!["-u".into(), "-c".into(), LIFECYCLE_PEER.into()],
        )
        .with_env([
            ("MODE", "startup".to_string()),
            ("STARTS", root.path().join("starts").display().to_string()),
        ]),
    )
    .with_timeouts(
        Duration::from_millis(500),
        Duration::from_millis(20),
        Duration::from_millis(70),
    );
    config.call_policy = McpCallPolicy {
        call_timeout_ms: 20,
        call_max_total_timeout_ms: 70,
        reconnect_max_attempts: ReconnectAttempts::Disabled,
        reconnect_initial_backoff_ms: 1,
        reconnect_max_backoff_ms: 1,
        liveness_probe_interval_ms: 5,
        ..McpCallPolicy::standard()
    };
    config.shutdown_policy = McpShutdownPolicy {
        graceful_period: Duration::from_millis(5),
        post_kill_wait: Duration::from_millis(5),
        scheduling_margin: Duration::from_millis(20),
        ..McpShutdownPolicy::standard()
    };
    let pool = McpConnectionPool::connect(BTreeMap::from([("startup".into(), config)]))
        .await
        .expect("valid configuration keeps the failed entry");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let status = &pool.server_statuses()[0];
    assert!(
        matches!(status.health, McpServerHealth::Disconnected { .. }),
        "{status:?}"
    );
    assert!(
        status
            .health
            .error()
            .expect("startup fault")
            .contains("500ms")
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("starts")).expect("starts"),
        "start\n"
    );
    pool.shutdown_all().await;
}

/// D-DEFAULTS2 / FIG-5494: facade shutdown timing reaches forced cleanup,
/// including child-exit polling on a host runtime without a signal driver.
#[cfg(target_os = "linux")]
#[test]
fn mcp_shutdown_policy_controls_forced_child_cleanup() {
    use lash::mcp::{McpConnectionPool, McpShutdownPolicy};
    let root = tempfile::tempdir().expect("fixture");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("host runtime without a signal driver");
    runtime.block_on(async {
        let policy = McpShutdownPolicy {
            graceful_period: Duration::from_millis(20),
            post_kill_wait: Duration::from_millis(30),
            scheduling_margin: Duration::from_secs(1),
            child_exit_poll_interval: Duration::from_millis(2),
        };
        let config = McpServerConfig::stdio(
            McpStdioTransport::new(
                "python3",
                vec!["-u".into(), "-c".into(), LIFECYCLE_PEER.into()],
            )
            .with_env([
                ("MODE", "shutdown".to_string()),
                ("STARTS", root.path().join("starts").display().to_string()),
            ]),
        )
        .with_shutdown_policy(policy);
        let pool = McpConnectionPool::connect(BTreeMap::from([("cleanup".into(), config)]))
            .await
            .expect("connect");
        assert!(pool.server_statuses()[0].health.is_connected());
        let started = tokio::time::Instant::now();
        tokio::time::timeout(
            policy.total_bound() + Duration::from_secs(5),
            pool.shutdown_all(),
        )
        .await
        .expect("configured cleanup completes");
        assert!(
            started.elapsed() >= policy.graceful_period + policy.post_kill_wait,
            "the server ignores EOF and SIGTERM, so both configured stages must elapse"
        );
        assert!(pool.server_statuses().is_empty());
    });
}
