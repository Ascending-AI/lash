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
use lash::mcp::{McpPluginFactory, McpServerConfig, McpStdioTransport};
use lash::plugins::PluginFactory;
use lash::provider::LlmResponse;
use lash_postgres_store::{PostgresStorage, PostgresStoreSet, testing::IsolatedDatabase};

const PEER: &str = r#"
import json, os, sys, threading, time
lock = threading.Lock()
started = threading.Event()
pending = []
root = os.environ['FIXTURE_ROOT']

def send(message):
    with lock:
        sys.stdout.write(json.dumps(message, separators=(',', ':')) + '\n')
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
                lash_restate_test::ServerConfig::default(),
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
                .with_env([("FIXTURE_ROOT", root.path().display().to_string())]),
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
        factory.server_statuses()[0].connected,
        "{:?}",
        factory.server_statuses()
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let name = lash::mcp::mcp_tool_name("catalog", "storm");
    let provider = lash_core::testing::TestProvider::builder()
        .complete(move |_| {
            let part = match observed.fetch_add(1, Ordering::SeqCst) {
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
                    parts: vec![part],
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    let core = lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .provider(provider)
        .model(
            lash::ModelSpec::builder("catalog-fixture")
                .context_window_tokens(16_000)
                .build()
                .expect("model"),
        )
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
    let session = crate::created_session(&core, "catalog-storm")
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
    assert_eq!(output.result.tool_calls.len(), 1);
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
#[ignore = "requires the managed PostgreSQL service gate"]
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
