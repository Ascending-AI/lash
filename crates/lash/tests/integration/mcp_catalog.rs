#![cfg(feature = "mcp")]
#![allow(clippy::disallowed_methods)]
#![expect(
    clippy::expect_used,
    reason = "integration fixture setup and assertions"
)]

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash::direct::LlmOutputPart;
use lash::mcp::{McpPluginFactory, McpServerConfig, McpServerHealth, McpStdioTransport};
use lash::plugins::PluginFactory;
use lash::provider::LlmResponse;
use lash_postgres_store::{PostgresStorage, PostgresStoreSet, testing::IsolatedDatabase};

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
    let double = lash_restate_test::backend(4548, Default::default())
        .await
        .expect("prompt backend");
    let mut observations = Vec::new();
    for surface in ["standard", "cell", "native"] {
        let protocol: Arc<dyn PluginFactory> = if surface == "standard" {
            Arc::new(lash_protocol_standard::StandardProtocolPluginFactory::new())
        } else {
            Arc::new(
                lash_protocol_rlm::RlmProtocolPluginFactory::new(
                    lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                        .channel(if surface == "cell" {
                            lash_protocol_rlm::RlmChannel::Cell
                        } else {
                            lash_protocol_rlm::RlmChannel::NativeTool
                        })
                        .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1000))
                        .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(1))
                        .build(),
                    Arc::new(lash_protocol_rlm::TypescriptDialect),
                    &double.lash_backend(),
                )
                .with_process_lifecycle(false),
            )
        };
        let session = lash_core::facade_support::PluginHost::new(vec![protocol, factory.clone()])
            .build_session(lash_core::plugin::PluginSessionRequest::creation(
                surface,
                Default::default(),
            ))
            .expect("prompt session");
        let catalog = session.resolved_tool_catalog().expect("captured catalog");
        // The recorded catalog is sufficient to rebuild the prompt without consulting the peer.
        let recorded = serde_json::to_vec(catalog.as_ref()).expect("record catalog");
        let catalog = serde_json::from_slice(&recorded).expect("restore catalog");
        let prompt = session
            .protocol_session()
            .render_system_prompt(lash::plugins::SystemPromptContext {
                plugin_config: &session.admitted_plugin_config(),
                tool_catalog: &catalog,
                subagent: None,
                purpose: lash::plugins::SystemPromptPurpose::Turn,
            })
            .await
            .expect("the protocol renders its system prompt");
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

async fn native_listener() -> tokio::net::TcpListener {
    let registry =
        std::env::var_os("MCP_CATALOG_ENDPOINTS_FILE").expect("managed gate endpoint registry");
    let used = std::fs::read_to_string(&registry).expect("recorded endpoint addresses");
    loop {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("native endpoint");
        let address = listener.local_addr().expect("endpoint address").to_string();
        if used.lines().any(|used| used == address) {
            continue;
        }
        std::fs::OpenOptions::new()
            .append(true)
            .open(&registry)
            .expect("endpoint registry")
            .write_all(format!("{address}\n").as_bytes())
            .expect("record unique endpoint address");
        return listener;
    }
}

async fn make_stores(
    store: Store,
    root: &std::path::Path,
    storage: Option<&PostgresStorage>,
    clock: Arc<dyn lash_core::Clock>,
) -> Arc<dyn lash_core::StoreSet> {
    match store {
        Store::SqliteMemory => Arc::new(
            lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
                .await
                .expect("SQLite memory"),
        ),
        Store::SqliteFile => Arc::new(
            lash_sqlite_store::SqliteStoreSet::open_with_clock(root.join("stores"), clock)
                .await
                .expect("SQLite file"),
        ),
        Store::Postgres => Arc::new(PostgresStoreSet::with_clock(
            storage.expect("PostgreSQL storage"),
            Arc::new(lash::persistence::FileAttachmentStore::new(
                root.join("attachments"),
            )),
            lash_core::WakeDeliveryConfig::default(),
            clock,
        )),
    }
}

async fn witness(store: Store, native: bool) {
    turn_witness(store, native, false).await;
}

async fn turn_witness(store: Store, native: bool, failure_law: bool) {
    println!(
        "host load: {}",
        std::fs::read_to_string("/proc/loadavg").expect("host load")
    );
    let root = tempfile::tempdir().expect("fixture directory");
    let database = if matches!(store, Store::Postgres) {
        let url = std::env::var("LASH_POSTGRES_DATABASE_URL").expect("managed PostgreSQL gate URL");
        Some(IsolatedDatabase::create(&url).await)
    } else {
        None
    };
    let storage = if let Some(database) = &database {
        Some(
            PostgresStorage::connect(database.url())
                .await
                .expect("PostgreSQL storage"),
        )
    } else {
        None
    };
    let double;
    let engine;
    let backend = if native {
        double = None;
        let namespace = format!("mcp-catalog-{}", uuid::Uuid::new_v4().simple());
        let config = lash_restate::RestateConfig::new(
            lash_restate::RestateConnection::new(
                std::env::var("RESTATE_INGRESS_URL").expect("native ingress URL"),
            ),
            lash_restate::RestateConnection::new(
                std::env::var("RESTATE_ADMIN_URL").expect("native admin URL"),
            ),
            lash_restate::RestateAuthorityId::new(&namespace).expect("authority"),
            lash_core::engine::BuildGeneration::for_test("mcp-catalog"),
        )
        .with_namespace(lash_restate::RestateNamespace::new(namespace).expect("namespace"));
        let stores = make_stores(
            store,
            root.path(),
            storage.as_ref(),
            Arc::new(lash_core::facade_support::SystemClock),
        )
        .await;
        engine = Some(Arc::new(lash_restate::RestateEngine::new(stores, config)));
        lash_core::Backend::new(engine.as_ref().expect("native engine").clone())
    } else {
        engine = None;
        double = Some(
            lash_restate_test::backend_with_store_set(
                4296,
                lash_restate_test::ServerConfig::default().always_replay(failure_law),
                lash_restate_test::DeploymentHooks::default(),
                |clock| async {
                    Ok(make_stores(store, root.path(), storage.as_ref(), clock).await)
                },
            )
            .await
            .expect("current turn backend"),
        );
        double.as_ref().expect("turn backend").lash_backend()
    };
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
        .models(std::sync::Arc::new(
            lash::ModelRegistry::new()
                .register(
                    "catalog-fixture",
                    lash::RegisteredModel::new(
                        lash::ModelMetadata::builder("catalog-fixture")
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
        .plugin(factory.clone())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "mcp-catalog",
            uuid::Uuid::new_v4().to_string(),
        ))
        .expect("core");
    let endpoint;
    let stop;
    if let Some(engine) = &engine {
        let processes = lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config().expect("worker config"),
        )
        .expect("worker");
        let listener = native_listener().await;
        let url = format!(
            "http://{}",
            listener.local_addr().expect("endpoint address")
        );
        let services = engine.endpoint_builder(processes).build();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        stop = Some(sender);
        endpoint = Some(tokio::spawn(async move {
            lash_restate::serve_endpoint(
                listener,
                services,
                lash_restate::RestateEndpointLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024 + 8),
                async {
                    let _ = receiver.await;
                },
            )
            .await;
        }));
        engine
            .register_deployment(&url)
            .await
            .expect("register native endpoint");
    } else {
        endpoint = None;
        stop = None;
    }
    let session = crate::created_session(&core, "catalog-fixture", "catalog-storm")
        .await
        .open()
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
        if failure_law { 2 } else { 1 }
    );
    if failure_law {
        let persisted = session
            .durable()
            .read()
            .await
            .expect("durable read")
            .expect("persisted session");
        assert_eq!(persisted.turn_index(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        if let Some(double) = &double {
            assert!(
                double
                    .server()
                    .invocations()
                    .iter()
                    .any(|view| view.suspensions > 0),
                "the failure turn must replay its journal"
            );
        }
        factory.shutdown().await.expect("MCP shutdown");
        if let Some(stop) = stop {
            let _ = stop.send(());
        }
        if let Some(endpoint) = endpoint {
            endpoint.await.expect("endpoint joins");
        }
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
            assert_eq!(error.retry, lash::tools::ToolRetryStatus::Never);
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
        .session("catalog-storm")
        .open()
        .await
        .expect("reopen committed session");
    assert!(reopened.read_view().chronological_projection().into_entries().iter().any(|entry| {
        matches!(&entry.payload, lash::persistence::ChronologicalPayload::Message(message) if lash::message_text(message).contains("storm done"))
    }), "the completed turn survives a store reload");
    factory.shutdown().await.expect("MCP shutdown");
    if let Some(stop) = stop {
        let _ = stop.send(());
    }
    if let Some(endpoint) = endpoint {
        endpoint.await.expect("native endpoint joins");
    }
    drop(core);
    drop(double);
    drop(engine);
    drop(database);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_storm_sqlite_memory_turn_witness() {
    witness(Store::SqliteMemory, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_storm_sqlite_file_turn_witness() {
    witness(Store::SqliteFile, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn catalog_storm_postgres_turn_witness() {
    witness(Store::Postgres, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the managed native Restate service gate"]
async fn catalog_storm_native_sqlite_memory_turn_witness() {
    witness(Store::SqliteMemory, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the managed native Restate service gate"]
async fn catalog_storm_native_sqlite_file_turn_witness() {
    witness(Store::SqliteFile, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the managed native Restate and PostgreSQL service gate"]
async fn catalog_storm_native_postgres_turn_witness() {
    witness(Store::Postgres, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_law_turn_failures_sqlite_memory() {
    turn_witness(Store::SqliteMemory, false, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_law_turn_failures_sqlite_file() {
    turn_witness(Store::SqliteFile, false, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn mcp_law_turn_failures_postgres() {
    turn_witness(Store::Postgres, false, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the managed PostgreSQL and live Restate service gate"]
async fn mcp_law_turn_failures_postgres_live() {
    turn_witness(Store::Postgres, true, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the managed live Restate service gate"]
async fn mcp_law_turn_failures_sqlite_memory_live() {
    turn_witness(Store::SqliteMemory, true, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the managed live Restate service gate"]
async fn mcp_law_turn_failures_sqlite_file_live() {
    turn_witness(Store::SqliteFile, true, true).await;
}
