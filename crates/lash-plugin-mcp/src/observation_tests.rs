//! MCP-LIVE: protocol facts reach a subscribed host, coalesce and never replay.
#![allow(
    deprecated,
    reason = "MCP logging remains an observed wire notification"
)]
use crate::{McpConnectionPool, McpServerConfig, McpStreamableHttpTransport};
use futures_util::StreamExt;
use lash_core::{LiveReplayStore, McpLiveItem, McpObservation, SessionId};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{RoleServer, ServerHandler, model::*, service::RequestContext};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

#[derive(Clone)]
struct ObservedServer {
    logs: usize,
    correlated: bool,
}
impl ServerHandler for ObservedServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: vec![Tool::new_with_raw(
                "work",
                None,
                json!({"type":"object"}).as_object().unwrap().clone(),
            )],
            ..Default::default()
        })
    }
    async fn call_tool(
        &self,
        _request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let token = context
            .meta
            .get_progress_token()
            .or_else(|| {
                context
                    .extensions
                    .get::<RequestMetaObject>()
                    .and_then(RequestMetaObject::get_progress_token)
            })
            .expect("call carries SDK progress token");
        for progress in 1..=3 {
            let mut params = ProgressNotificationParam::new(token.clone(), f64::from(progress));
            params.total = Some(3.0);
            params.message = Some(format!("step {progress}"));
            context.peer.notify_progress(params).await.unwrap();
        }
        for _ in 0..self.logs {
            let mut log = LoggingMessageNotificationParam::new(
                LoggingLevel::Info,
                json!({"message":"working"}),
            )
            .with_logger("fixture");
            if self.correlated {
                log.meta = Some(NotificationMetaObject::from(
                    json!({"progressToken":token}).as_object().unwrap().clone(),
                ));
            }
            context.peer.notify_logging_message(log).await.unwrap();
        }
        Ok(CallToolResult::success(vec![ContentBlock::text("ok")]).into())
    }
}

#[tokio::test]
async fn http_progress_and_log_are_correlated_coalesced_and_live_only() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let service = StreamableHttpService::new(
            || {
                Ok(ObservedServer {
                    logs: 1,
                    correlated: true,
                })
            },
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );
        let app = axum::Router::new().nest_service("/mcp", service);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let pool = McpConnectionPool::connect(BTreeMap::from([(
            "fixture".into(),
            McpServerConfig::streamable_http(McpStreamableHttpTransport::new(format!(
                "http://{address}/mcp"
            ))),
        )]))
        .await
        .unwrap();
        assert_live_call(&pool).await;
        pool.shutdown_all().await;
        server.abort();
        let _ = server.await;
    })
    .await
    .expect("bounded HTTP observation law");
}

async fn assert_live_call(pool: &McpConnectionPool) {
    let replay = lash_core::facade_support::InMemoryLiveReplayStore::new(
        lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
    );
    let hub = replay.mcp_observations().unwrap();
    let session = SessionId::from("observed-call");
    let cursor = replay.current_cursor(&session, lash_core::SessionRevision::new(0));
    let mut feed = hub.subscribe_session(session.clone());
    assert!(matches!(feed.next().await, Some(McpLiveItem::Gap(_))));
    let mut witness = hub.subscribe_session(session.clone());
    let _ = witness.next().await;
    let context =
        lash_core::testing::mock_attempt_context().with_mcp_publisher(hub.session(session.clone()));
    let call = lash_core::McpCallIdentity {
        call_id: context.call_id().clone(),
        attempt: context.attempt_number(),
    };
    let outcome = pool
        .call_tool(pool.advertised_tools()[0].name(), &json!({}), &context)
        .await;
    assert!(outcome.is_success(), "{outcome:?}");
    // The response and notifications are separate SDK tasks. Wait for the
    // log, then read the coalesced progress that was queued before it.
    let mut progress = None;
    let mut log = None;
    while log.is_none() || progress.as_ref().is_none_or(|(value, _, _)| *value != 3.0) {
        match witness.next().await.unwrap() {
            McpLiveItem::Observation(McpObservation::McpProgress {
                server,
                call: observed,
                progress: value,
                total,
                message,
            }) => {
                assert_eq!(server, "fixture");
                assert_eq!(observed, call);
                progress = Some((value, total, message));
            }
            McpLiveItem::Observation(McpObservation::McpLog {
                server,
                call: observed,
                level,
                logger,
                data,
            }) => {
                assert_eq!(server, "fixture");
                assert_eq!(observed, Some(call.clone()));
                log = Some((level, logger, data));
            }
            other => panic!("unexpected observation: {other:?}"),
        }
    }
    assert_eq!(progress, Some((3.0, Some(3.0), Some("step 3".into()))));
    let mut facts = vec![feed.next().await.unwrap(), feed.next().await.unwrap()];
    assert!(facts.iter().any(|item| matches!(
        item,
        McpLiveItem::Observation(McpObservation::McpProgress { progress: 3.0, .. })
    )));
    assert!(facts.iter().any(|item| matches!(
        item,
        McpLiveItem::Observation(McpObservation::McpLog { .. })
    )));
    facts.clear();
    assert!(
        futures_util::poll!(feed.next()).is_pending(),
        "pending progress coalesces to one latest value"
    );
    assert_eq!(
        log,
        Some((
            lash_core::McpLogLevel::Info,
            Some("fixture".into()),
            json!({"message":"working"})
        ))
    );
    let lash_core::LiveReplayOutcome::Replayed(events) =
        replay.replay_after_cursor(&cursor).await.unwrap()
    else {
        panic!("no replay loss");
    };
    assert!(events.is_empty(), "no MCP row enters the replay cache");
    let mut reconnected = hub.subscribe_session(session);
    assert!(matches!(
        reconnected.next().await,
        Some(McpLiveItem::Gap(lash_core::McpObservationGap::Unavailable))
    ));
    assert!(
        futures_util::poll!(reconnected.next()).is_pending(),
        "there is no MCP history"
    );
}

const STDIO_SERVER: &str = r#"
import json,sys
def send(value): print(json.dumps(value),flush=True)
for line in sys.stdin:
    message=json.loads(line)
    method=message.get('method')
    if method=='server/discover':
        result={'supportedVersions':['2026-07-28'],'resultType':'complete','ttlMs':0,'cacheScope':'private','capabilities':{'tools':{}},'serverInfo':{'name':'fixture','version':'1'}}
    elif method=='tools/list':
        result={'resultType':'complete','tools':[{'name':'work','inputSchema':{'type':'object'}}],'ttlMs':0,'cacheScope':'private'}
    elif method=='tools/call':
        token=message['params']['_meta']['progressToken']
        for progress in range(1,4):
            send({'jsonrpc':'2.0','method':'notifications/progress','params':{'progressToken':token,'progress':progress,'total':3,'message':'step '+str(progress)}})
        send({'jsonrpc':'2.0','method':'notifications/message','params':{'level':'info','logger':'fixture','data':{'message':'working'},'_meta':{'progressToken':token}}})
        result={'content':[{'type':'text','text':'ok'}]}
    else: continue
    send({'jsonrpc':'2.0','id':message['id'],'result':result})
"#;

#[tokio::test]
async fn stdio_progress_and_log_are_correlated_coalesced_and_live_only() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let pool = McpConnectionPool::connect(BTreeMap::from([(
            "fixture".into(),
            McpServerConfig::stdio(crate::McpStdioTransport::new(
                "python3",
                vec!["-u".into(), "-c".into(), STDIO_SERVER.into()],
            )),
        )]))
        .await
        .unwrap();
        assert_live_call(&pool).await;
        pool.shutdown_all().await;
    })
    .await
    .expect("bounded stdio observation law");
}

/// MCP-BOUNDS: unread per-server logs retain exactly the configured bound
/// and report every omission, without altering the tool result.
#[tokio::test]
async fn server_logs_are_bounded_and_overflow_reports_the_dropped_count() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let service = StreamableHttpService::new(
            || {
                Ok(ObservedServer {
                    logs: 5,
                    correlated: false,
                })
            },
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );
        let app = axum::Router::new().nest_service("/mcp", service);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let factory = crate::McpPluginFactory::builder(BTreeMap::from([(
            "fixture".into(),
            McpServerConfig::streamable_http(McpStreamableHttpTransport::new(format!(
                "http://{address}/mcp"
            ))),
        )]))
        .observation_policy(lash_core::McpObservationPolicy::new(2, 64, 64, 65536).unwrap())
        .build()
        .await
        .unwrap();
        let mut feed = factory.observe_server("fixture");
        let mut witness = factory.observe_server("fixture");
        let _ = feed.next().await;
        let _ = witness.next().await;
        let outcome = factory
            .pool()
            .call_tool(
                factory.pool().advertised_tools()[0].name(),
                &json!({}),
                &lash_core::testing::mock_attempt_context(),
            )
            .await;
        assert!(outcome.is_success(), "{outcome:?}");
        let mut accounted = 0;
        while accounted < 5 {
            match witness.next().await.unwrap() {
                McpLiveItem::Observation(McpObservation::McpLog { call: None, .. }) => {
                    accounted += 1
                }
                McpLiveItem::Observation(McpObservation::McpObservationsDropped {
                    kind: lash_core::McpObservationKind::Log,
                    count,
                    ..
                }) => accounted += count,
                other => panic!("unexpected server fact: {other:?}"),
            }
        }
        for _ in 0..2 {
            assert!(matches!(
                feed.next().await,
                Some(McpLiveItem::Observation(McpObservation::McpLog {
                    call: None,
                    ..
                }))
            ));
        }
        assert_eq!(
            feed.next().await,
            Some(McpLiveItem::Observation(
                McpObservation::McpObservationsDropped {
                    server: "fixture".into(),
                    kind: lash_core::McpObservationKind::Log,
                    count: 3
                }
            ))
        );
        assert!(futures_util::poll!(feed.next()).is_pending());
        factory.pool().shutdown_all().await;
        server.abort();
        let _ = server.await;
    })
    .await
    .expect("bounded log law");
}

/// MCP-ROUTING: the engine supplies the executing session's feed; the host
/// registers the plugin and subscribes normally, with no per-session wiring.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_dispatch_routes_mcp_facts_without_host_wiring() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let factory = Arc::new(
            crate::McpPluginFactory::new(BTreeMap::from([(
                "fixture".into(),
                McpServerConfig::stdio(crate::McpStdioTransport::new(
                    "python3",
                    vec!["-u".into(), "-c".into(), STDIO_SERVER.into()],
                )),
            )]))
            .await
            .unwrap(),
        );
        let name = factory.pool().advertised_tools()[0].name().to_string();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = lash_core::testing::TestProvider::builder()
            .complete(move |_| {
                let part = if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    lash::direct::LlmOutputPart::ToolCall {
                        call_id: "provider-id".into(),
                        tool_name: name.clone(),
                        input_json: "{}".into(),
                        replay: None,
                    }
                } else {
                    lash::direct::LlmOutputPart::Text {
                        text: "done".into(),
                        response_meta: None,
                    }
                };
                async move {
                    Ok(lash::provider::LlmResponse {
                        parts: vec![part],
                        ..Default::default()
                    })
                }
            })
            .build()
            .into_handle();
        let stores = Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap());
        let backend = lash::durable::DurableBackendBuilder::new(stores)
            .build()
            .unwrap();
        let core = lash::LashCore::standard_builder(backend)
            .llm_profiles(lash::testing::single_llm_profile_registry(
                "fixture-model",
                lash::testing::test_llm_profile_metadata("fixture-model"),
                provider,
            ))
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .data_retention(lash::DataRetention::standard())
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .execution_budgets(lash::ExecutionBudgets::recommended())
            .delta_coalescing(lash::DeltaCoalescing::recommended())
            .plugin(factory.clone())
            .build(lash_core::testing::runtime_lease_owner())
            .unwrap();
        let id = SessionId::from("automatic-mcp");
        core.session(id.clone())
            .create(lash::SessionCreation::root(
                lash::SessionSpec::new(
                    "fixture-model",
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(16),
                )
                .no_progress_budget(lash::NoProgressBudget::bounded(12)),
            ))
            .await
            .unwrap();
        let session = core.session(id).open().await.unwrap();
        let snapshot = session.observe().snapshot().await.unwrap();
        let mut feed = session.observe().subscribe_and_recover(snapshot.cursor);
        assert!(matches!(
            feed.next().await.unwrap().unwrap(),
            lash::observe::SessionObservationStreamItem::McpGap(
                lash_core::McpObservationGap::Unavailable
            )
        ));
        let output = session
            .send(lash::TurnInput::text("work"))
            .output()
            .await
            .unwrap();
        let record = &output.result.tool_calls[0];
        assert!(record.output.is_success(), "{:?}", record.output);
        let mut saw_progress = false;
        let mut saw_log = false;
        while !saw_progress || !saw_log {
            match feed.next().await.unwrap().unwrap() {
                lash::observe::SessionObservationStreamItem::Mcp(McpObservation::McpProgress {
                    call,
                    progress,
                    ..
                }) => {
                    assert_eq!(call.call_id, record.call_id);
                    assert_eq!(call.attempt, 1);
                    saw_progress |= progress == 3.0;
                }
                lash::observe::SessionObservationStreamItem::Mcp(McpObservation::McpLog {
                    call: Some(call),
                    ..
                }) => {
                    assert_eq!(call.call_id, record.call_id);
                    saw_log = true;
                }
                _ => {}
            }
        }
        core.shutdown().await.unwrap();
    })
    .await
    .expect("automatic observation route");
}

/// MCP-LIST-CHANGED: unsolicited list notices remain server scoped and
/// preserve the three protocol lists; they neither fan out nor start a turn.
#[tokio::test]
async fn server_list_changes_preserve_the_list_and_stay_server_scoped() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let script = STDIO_SERVER.replace(
            "token=message['params']['_meta']['progressToken']",
            "for method in ['notifications/tools/list_changed','notifications/resources/list_changed','notifications/prompts/list_changed']:\n            send({'jsonrpc':'2.0','method':method})\n        token=message['params']['_meta']['progressToken']",
        );
        let factory = crate::McpPluginFactory::new(BTreeMap::from([(
            "fixture".into(),
            McpServerConfig::stdio(crate::McpStdioTransport::new(
                "python3", vec!["-u".into(), "-c".into(), script],
            )),
        )])).await.unwrap();
        let mut feed = factory.observe_server("fixture");
        let _ = feed.next().await;
        let output = factory.pool().call_tool(
            factory.pool().advertised_tools()[0].name(), &json!({}),
            &lash_core::testing::mock_attempt_context(),
        ).await;
        assert!(output.is_success(), "{output:?}");
        let mut lists = Vec::new();
        for _ in 0..3 {
            let Some(McpLiveItem::Observation(McpObservation::McpListChanged { server, list })) = feed.next().await else { panic!("typed list notice"); };
            assert_eq!(server, "fixture");
            lists.push(list);
        }
        assert!(lists.contains(&lash_core::McpList::Tools));
        assert!(lists.contains(&lash_core::McpList::Resources));
        assert!(lists.contains(&lash_core::McpList::Prompts));
        assert!(futures_util::poll!(feed.next()).is_pending());
        factory.pool().shutdown_all().await;
    }).await.expect("server list observations");
}
