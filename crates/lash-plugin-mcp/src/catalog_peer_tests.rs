use super::*;
use std::sync::atomic::AtomicUsize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Notify;

#[derive(Clone, Copy)]
enum Scenario {
    Storm,
    Cycle,
    Pages,
    Items,
    Bytes,
    Valid,
}

struct PeerState {
    scenario: Scenario,
    initializations: AtomicUsize,
    calls: AtomicUsize,
    stalled: Notify,
    release: Notify,
}

struct HttpPeer {
    url: String,
    state: Arc<PeerState>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for HttpPeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl HttpPeer {
    async fn start(scenario: Scenario) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("HTTP peer listener");
        let url = format!(
            "http://{}/mcp",
            listener.local_addr().expect("HTTP address")
        );
        let state = Arc::new(PeerState {
            scenario,
            initializations: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            stalled: Notify::new(),
            release: Notify::new(),
        });
        let shared = Arc::clone(&state);
        let task = tokio::spawn(async move {
            let mut requests = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.expect("HTTP accept");
                        let state = Arc::clone(&shared);
                        requests.spawn(answer(stream, state));
                    }
                    result = requests.join_next(), if !requests.is_empty() => {
                        result.expect("request result").expect("HTTP request task");
                    }
                }
            }
        });
        Self { url, state, task }
    }

    fn config(&self) -> McpServerConfig {
        McpServerConfig::streamable_http(crate::config::McpStreamableHttpTransport::new(&self.url))
            .with_timeouts(
                Duration::from_secs(60),
                Duration::from_secs(60),
                Duration::from_secs(120),
            )
    }
}

async fn answer(stream: tokio::net::TcpStream, state: Arc<PeerState>) {
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    stream.read_line(&mut line).await.expect("HTTP method");
    let post = line.starts_with("POST ");
    let mut length = 0;
    loop {
        line.clear();
        stream.read_line(&mut line).await.expect("HTTP header");
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().expect("content length");
        }
    }
    if !post {
        stream.get_mut().write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.expect("reject SSE GET");
        return;
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.expect("HTTP body");
    let request: Value = serde_json::from_slice(&body).expect("MCP JSON request");
    let id = &request["id"];
    let method = request["method"].as_str().expect("MCP method");
    let mut content_type = "application/json";
    let body = match method {
        "initialize" => json!({"jsonrpc":"2.0", "id": id, "result": {
            "protocolVersion":"2025-11-25", "capabilities":{"tools":{"listChanged":true}},
            "serverInfo":{"name":"catalog-http-peer","version":"1"},
            "instructions": format!("HTTP guidance generation {}", state.initializations.fetch_add(1, Ordering::SeqCst) + 1)
        }})
        .to_string(),
        "tools/list" => {
            let index = state.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if matches!(state.scenario, Scenario::Storm) && index == 2 {
                let body = format!(
                    "event: message\ndata: {}\n\n",
                    json!({
                        "jsonrpc":"2.0", "id":id, "result":{"tools":[{
                            "name":"work-2", "inputSchema":{"type":"object"}
                        }]}
                    })
                );
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream
                    .get_mut()
                    .write_all(headers.as_bytes())
                    .await
                    .expect("open discovery SSE stream");
                state.stalled.notify_one();
                state.release.notified().await;
                let _ = stream.get_mut().write_all(body.as_bytes()).await;
                return;
            }
            let mut result = json!({"tools": [{"name": format!("work-{index}"), "inputSchema":{"type":"object"}}]});
            match state.scenario {
                Scenario::Cycle => {
                    result = json!({"tools": []});
                    if index < 3 {
                        result["nextCursor"] = json!("cycle");
                    }
                }
                Scenario::Pages => {
                    result = json!({"tools": []});
                    if index < 65 {
                        result["nextCursor"] = json!(index.to_string());
                    }
                }
                Scenario::Items => {
                    result["tools"] = json!((0..4097).map(|i| json!({"name": format!("work-{i}"), "inputSchema":{"type":"object"}})).collect::<Vec<_>>());
                }
                Scenario::Bytes => {
                    result["tools"][0]["description"] = json!("x".repeat(8 * 1024 * 1024 + 1));
                }
                Scenario::Storm | Scenario::Valid => {}
            }
            json!({"jsonrpc":"2.0", "id":id, "result":result}).to_string()
        }
        "tools/call" => {
            content_type = "text/event-stream";
            let notifications = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n".repeat(100_000);
            format!(
                "{notifications}event: message\ndata: {}\n\n",
                json!({"jsonrpc":"2.0", "id":id, "result":{"content":[{"type":"text","text":"storm sent"}]}})
            )
        }
        _ => String::new(),
    };
    let status = if body.is_empty() {
        "202 Accepted"
    } else {
        "200 OK"
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    // Shutdown may close a deliberately stalled response's socket.
    let _ = stream.get_mut().write_all(response.as_bytes()).await;
}

#[tokio::test]
async fn http_catalog_storm_coalesces_wire_notifications_and_keeps_control_live() {
    let fixture = HttpPeer::start(Scenario::Storm).await;
    let pool = McpConnectionPool::connect(BTreeMap::from([("http".to_string(), fixture.config())]))
        .await
        .expect("connect HTTP peer");
    let entry = pool.entries.read_recover()["http"].clone();
    let service = entry.service_snapshot().expect("HTTP service");
    entry.request_tool_refresh(service.generation);
    fixture.state.stalled.notified().await;
    service
        .peer
        .call_tool(CallToolRequestParams::new("storm"))
        .await
        .expect("wire storm call completes while refresh stalls");
    while entry.refresh_notifications.load(Ordering::SeqCst) < 100_001 {
        tokio::task::yield_now().await;
    }
    entry
        .establish()
        .await
        .expect("control barrier during HTTP storm");
    assert_eq!(fixture.state.calls.load(Ordering::SeqCst), 2);
    fixture.state.release.notify_one();
    while pool.advertised_tools()[0].name() != naming::build_prefixed_name("http", "work-3").0 {
        tokio::task::yield_now().await;
    }
    entry
        .establish()
        .await
        .expect("control barrier after HTTP repeat");
    assert_eq!(fixture.state.calls.load(Ordering::SeqCst), 3);
    pool.shutdown_all().await;
}

#[tokio::test]
async fn http_catalog_refuses_cycles_and_each_limit_plus_one() {
    for (scenario, reason) in [
        (Scenario::Cycle, "cursor cycle"),
        (Scenario::Pages, "page limit"),
        (Scenario::Items, "item limit"),
        (Scenario::Bytes, "byte limit"),
    ] {
        let fixture = HttpPeer::start(scenario).await;
        let pool =
            McpConnectionPool::connect(BTreeMap::from([("http".to_string(), fixture.config())]))
                .await
                .expect("retained HTTP entry");
        let status = pool.server_statuses().remove(0);
        pool.shutdown_all().await;
        assert!(!status.connected, "catalog was installed");
        assert!(
            status
                .last_error
                .expect("catalog fault")
                .message()
                .contains(reason)
        );
    }
}

#[tokio::test]
async fn http_valid_catalog_refresh_installs_once() {
    let fixture = HttpPeer::start(Scenario::Valid).await;
    let pool = McpConnectionPool::connect(BTreeMap::from([("http".to_string(), fixture.config())]))
        .await
        .expect("connected HTTP entry");
    let entry = pool.entries.read_recover()["http"].clone();
    entry.request_tool_refresh(entry.service_snapshot().expect("HTTP service").generation);
    while pool.advertised_tools()[0].name() != naming::build_prefixed_name("http", "work-2").0 {
        tokio::task::yield_now().await;
    }
    entry.establish().await.expect("publication barrier");
    assert_eq!(fixture.state.calls.load(Ordering::SeqCst), 2);
    pool.shutdown_all().await;
}

#[tokio::test]
async fn http_server_instructions_follow_catalog_refresh_and_reconnect() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let fixture = HttpPeer::start(Scenario::Valid).await;
        let pool =
            McpConnectionPool::connect(BTreeMap::from([("http".to_string(), fixture.config())]))
                .await
                .expect("connected HTTP entry");
        let instructions = || {
            serde_json::to_value(&pool.advertised_tools()[0].manifest).expect("manifest record")
                ["module"]["instructions"].clone()
        };
        let mut observed = vec![instructions()];
        let entry = pool.entries.read_recover()["http"].clone();
        let generation = entry.service_snapshot().expect("HTTP service").generation;
        entry.request_tool_refresh(generation);
        while pool.advertised_tools()[0].name() != naming::build_prefixed_name("http", "work-2").0 {
            tokio::task::yield_now().await;
        }
        observed.push(instructions());
        assert!(entry.mark_disconnected("instruction reconnect witness".to_string(), generation));
        observed.push(instructions());
        while entry
            .service_snapshot()
            .is_none_or(|service| service.generation == generation)
        {
            tokio::task::yield_now().await;
        }
        observed.push(instructions());
        pool.shutdown_all().await;
        assert_eq!(
            observed,
            [
                json!("HTTP guidance generation 1"),
                json!("HTTP guidance generation 1"),
                json!("HTTP guidance generation 1"),
                json!("HTTP guidance generation 2")
            ]
        );
    })
    .await
    .expect("bounded instruction lifecycle witness");
}

#[tokio::test]
async fn http_shutdown_cancels_stalled_catalog_discovery() {
    let fixture = HttpPeer::start(Scenario::Storm).await;
    let pool = McpConnectionPool::connect(BTreeMap::from([("http".to_string(), fixture.config())]))
        .await
        .expect("connect HTTP peer");
    let entry = pool.entries.read_recover()["http"].clone();
    entry.request_tool_refresh(entry.service_snapshot().expect("HTTP service").generation);
    fixture.state.stalled.notified().await;
    pool.shutdown_all().await;
    assert!(entry.service_snapshot().is_none());
    assert!(entry.actor_handle.lock_recover().is_none());
    assert_eq!(fixture.state.calls.load(Ordering::SeqCst), 2);
}
