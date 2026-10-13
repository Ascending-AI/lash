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
    subscription: tokio::sync::Mutex<Option<(Value, tokio::net::tcp::OwnedWriteHalf)>>,
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
            subscription: tokio::sync::Mutex::new(None),
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
    let mut headers = BTreeMap::new();
    loop {
        line.clear();
        stream.read_line(&mut line).await.expect("HTTP header");
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
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
    assert_eq!(
        headers.get("mcp-protocol-version").map(String::as_str),
        Some("2026-07-28")
    );
    assert_eq!(headers.get("mcp-method").map(String::as_str), Some(method));
    assert!(!headers.contains_key("mcp-session-id"));
    if method == "tools/call" {
        assert_eq!(
            headers.get("mcp-name").map(String::as_str),
            request["params"]["name"].as_str()
        );
    }
    if method == "subscriptions/listen" {
        let (_, mut writer) = stream.into_inner().into_split();
        writer.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        let ack = format!(
            "event: message\ndata: {}\n\n",
            json!({"jsonrpc":"2.0", "method":"notifications/subscriptions/acknowledged", "params":{"notifications":{"toolsListChanged":true},"_meta":{"io.modelcontextprotocol/subscriptionId":id}}})
        );
        write_chunk(&mut writer, &ack).await;
        *state.subscription.lock().await = Some((id.clone(), writer));
        return;
    }
    let content_type = "application/json";
    let body = match method {
        "server/discover" => json!({"jsonrpc":"2.0", "id": id, "result": {
            "supportedVersions":["2026-07-28"],"resultType":"complete","ttlMs":0,"cacheScope":"private", "capabilities":{"tools":{"listChanged":matches!(state.scenario, Scenario::Storm)}},
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
            let mut subscription = state.subscription.lock().await;
            let (subscription_id, writer) = subscription.as_mut().expect("acknowledged subscription");
            let notification = format!("event: message\ndata: {}\n\n", json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":{"_meta":{"io.modelcontextprotocol/subscriptionId":subscription_id}}}));
            write_chunk(writer, &notification.repeat(100_000)).await;
            json!({"jsonrpc":"2.0","id":id,"result":{"resultType":"complete","content":[{"type":"text","text":"storm sent"}]}}).to_string()
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

async fn write_chunk(writer: &mut tokio::net::tcp::OwnedWriteHalf, body: &str) {
    writer
        .write_all(format!("{:x}\r\n", body.len()).as_bytes())
        .await
        .unwrap();
    writer.write_all(body.as_bytes()).await.unwrap();
    writer.write_all(b"\r\n").await.unwrap();
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
    while pool.advertised_tools()[0].name()
        != crate::mcp_tool_names("http", &["work-3"])["work-3"].clone()
    {
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
        assert!(!status.health.is_connected(), "catalog was installed");
        assert!(
            status
                .health
                .error()
                .expect("catalog fault")
                .contains(reason)
        );
    }
}

/// The `mcp/server.*` sections a call offered `tools` renders, as
/// (section key, text).
fn guidance_sections(tools: Vec<lash_core::ToolDefinition>) -> Vec<(String, String)> {
    use lash_core::plugin::prompt::{OfferedTools, PromptCall, PromptPurpose, PromptSectionSource};
    use lash_core::testing::prompt::PromptCutParts;
    let offered = OfferedTools::new(
        Arc::new(lash_core::ToolCatalog::from_tool_definitions(tools)),
        false,
    );
    let cut = lash_core::testing::prompt::cut(PromptCutParts {
        call: PromptCall {
            session_id: lash_core::SessionId::from("mcp-cut"),
            frame: None,
            run: None,
            turn: None,
            iteration: 0,
            call: 0,
            purpose: PromptPurpose::Turn,
        },
        config: lash_core::OwnerConfigView::session(lash_core::AdmittedPluginConfig::default()),
        session: None,
        offered: offered.clone(),
        model: Default::default(),
        history: Default::default(),
        namespaces: BTreeMap::new(),
    });
    guidance::McpGuidanceSections
        .sections(&offered)
        .into_iter()
        .map(|section| {
            let text = section
                .renderer
                .render(&cut.input_for("mcp"))
                .expect("pinned guidance renders");
            (
                format!(
                    "{}.{}",
                    guidance::McpGuidanceSections::FAMILY,
                    section.suffix
                ),
                text.as_text().expect("guidance text").to_owned(),
            )
        })
        .collect()
}

/// MCP-CUT: server guidance is an admitted catalog resource. Every catalog
/// the pool advertises pins its server's guidance; a refresh keeps it and a
/// reconnect captures the new discover response. The server's section
/// renders exactly the guidance of the catalog a call was offered, and
/// rendering never contacts the server: every render runs after the pool
/// has shut down and the server is gone.
#[tokio::test]
async fn mcp_guidance_renders_the_offered_catalogs_revision_without_the_server() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let fixture = HttpPeer::start(Scenario::Valid).await;
        let pool =
            McpConnectionPool::connect(BTreeMap::from([("http".to_string(), fixture.config())]))
                .await
                .expect("connected HTTP entry");
        let connected = pool.advertised_tools();
        let entry = pool.entries.read_recover()["http"].clone();
        let generation = entry.service_snapshot().expect("HTTP service").generation;
        entry.request_tool_refresh(generation);
        while pool.advertised_tools()[0].name()
            != crate::mcp_tool_names("http", &["work-2"])["work-2"]
        {
            tokio::task::yield_now().await;
        }
        let refreshed = pool.advertised_tools();
        assert!(entry.mark_disconnected("guidance reconnect witness".to_string(), generation));
        while entry
            .service_snapshot()
            .is_none_or(|service| service.generation == generation)
        {
            tokio::task::yield_now().await;
        }
        let reconnected = pool.advertised_tools();
        pool.shutdown_all().await;
        let initializations = fixture.state.initializations.load(Ordering::SeqCst);
        drop(fixture);

        let section =
            |text: &str| vec![("server.http".to_string(), format!("#### http\n\n{text}"))];
        assert_eq!(
            guidance_sections(connected.clone()),
            section("HTTP guidance generation 1")
        );
        assert_eq!(
            guidance_sections(refreshed),
            section("HTTP guidance generation 1"),
            "a tool refresh keeps the connection's guidance"
        );
        assert_eq!(
            guidance_sections(reconnected),
            section("HTTP guidance generation 2"),
            "a reconnect pins the new discover response"
        );
        assert_eq!(
            guidance_sections(connected),
            section("HTTP guidance generation 1"),
            "the first catalog still renders its own revision"
        );
        assert_eq!(
            initializations, 2,
            "only the two connections met the server"
        );
        assert!(
            guidance_sections(Vec::new()).is_empty(),
            "no offered server, no section"
        );
    })
    .await
    .expect("bounded guidance witness");
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

/// MCP-DATA-POLICY: complete discovery respects deployment catalog bounds.
#[tokio::test]
async fn a_nondefault_catalog_limit_is_honored_at_discovery() {
    let fixture = HttpPeer::start(Scenario::Valid).await;
    let mut config = fixture.config();
    config.data_limits.catalog_items = 0;
    let result =
        McpConnectionPool::connect(BTreeMap::from([("configured".to_string(), config)])).await;
    // Pools retain a disconnected peer's typed operational health, rather than failing deployment construction.
    let pool = result.expect("pool");
    let health = pool.server_statuses();
    assert!(format!("{health:?}").contains("item limit 0"), "{health:?}");
    pool.shutdown_all().await;
}
