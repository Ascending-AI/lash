//! MCP-2026-ONLY and MCP-MRTR-HOST-INPUT laws.
#![allow(
    deprecated,
    reason = "these laws retain existing roots and sampling capabilities"
)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerConfig};
use rmcp::{ClientHandler, ClientServiceExt, ServerHandler, ServiceExt};
use serde_json::{Value, json};

use lash_core::facade_support::PluginFactory;

use crate::{
    MCP_PROTOCOL_VERSION, McpElicitationHandler, McpElicitationModes, McpElicitationParams,
    McpElicitationRequest, McpElicitationResult, McpHostRefusal, McpPluginFactory, McpRoot,
    McpRootsProvider, McpRootsRequest, McpSamplingHandler, McpSamplingRequest, McpSamplingResult,
    McpServerConfig, McpStdioTransport, McpToolProvider, McpTransport,
};

#[derive(Clone)]
struct RevisionServer(ProtocolVersion);
impl ServerHandler for RevisionServer {
    fn supported_protocol_versions(&self) -> std::borrow::Cow<'static, [ProtocolVersion]> {
        vec![self.0.clone()].into()
    }
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::default())
            .with_server_info(Implementation::new("revision-law", "1"))
    }
}

#[tokio::test]
async fn modern_discovery_negotiates_the_only_supported_revision() {
    let (client_io, server_io) = tokio::io::duplex(4096);
    let handler = crate::host::LashMcpClientHandler::new("revision", Default::default());
    let server = tokio::spawn(RevisionServer(ProtocolVersion::V_2026_07_28).serve(server_io));
    let client = handler
        .serve_with_lifecycle(client_io, crate::service_lifecycle::client_lifecycle())
        .await;
    let client = client.expect("modern discovery");
    assert_eq!(
        client.peer_info().unwrap().protocol_version.as_str(),
        MCP_PROTOCOL_VERSION
    );
    assert_eq!(
        client.service().get_info().protocol_version,
        ProtocolVersion::V_2026_07_28
    );
    client.cancel().await.unwrap();
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn older_only_server_is_refused_with_typed_revisions() {
    let root = tempfile::tempdir().unwrap();
    let trace = root.path().join("trace.jsonl");
    let mut configs = configs(&trace, "inputs");
    let McpTransport::Stdio(stdio) = &mut configs.get_mut("modern").unwrap().transport else {
        unreachable!()
    };
    stdio.env.insert("REVISION".into(), "2025-11-25".into());
    let factory = McpPluginFactory::new(configs).await.unwrap();
    let failure = factory
        .startup_failure("modern")
        .expect("revision refusal retained");
    let refusal = failure
        .protocol_refusal
        .expect("typed revisions survive cleanup");
    assert_eq!(refusal.expected, MCP_PROTOCOL_VERSION);
    assert_eq!(refusal.offered, vec!["2025-11-25".to_string()]);
    assert!(
        read_trace(&trace)
            .iter()
            .all(|request| request["method"] == "server/discover")
    );
    factory.shutdown().await.unwrap();
}

#[derive(Default)]
struct InputHost {
    calls: AtomicUsize,
}
#[async_trait]
impl McpSamplingHandler for InputHost {
    async fn create_message(
        &self,
        request: McpSamplingRequest<'_>,
    ) -> Result<McpSamplingResult, McpHostRefusal> {
        assert_eq!(request.context.server_name(), "modern");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(McpSamplingResult::text("sampled", "host-model"))
    }
}
#[async_trait]
impl McpElicitationHandler for InputHost {
    fn modes(&self) -> McpElicitationModes {
        McpElicitationModes::FormAndUrl
    }
    async fn create_elicitation(
        &self,
        request: McpElicitationRequest<'_>,
    ) -> Result<McpElicitationResult, McpHostRefusal> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match request.params {
            McpElicitationParams::Form { message, .. } if message == "invalid" => {
                Ok(McpElicitationResult::Accept {
                    content: Some(json!({"answer":true})),
                })
            }
            McpElicitationParams::Form { .. } => request
                .accept(json!({"answer":"yes"}))
                .map_err(McpHostRefusal::from),
            McpElicitationParams::Url { url, .. } => {
                assert_eq!(url, "https://example.invalid/approve");
                Ok(McpElicitationResult::Accept { content: None })
            }
        }
    }
}
#[async_trait]
impl McpRootsProvider for InputHost {
    async fn list_roots(&self, _: McpRootsRequest<'_>) -> Result<Vec<McpRoot>, McpHostRefusal> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![McpRoot::new("file:///workspace/law")])
    }
}

#[tokio::test]
async fn modern_tool_round_trip_preserves_sampling_form_url_and_roots() {
    let root = tempfile::tempdir().unwrap();
    let trace = root.path().join("trace.jsonl");
    let host = Arc::new(InputHost::default());
    let factory = McpPluginFactory::builder(configs(&trace, "inputs"))
        .sampling_handler(host.clone())
        .elicitation_handler(host.clone())
        .roots_provider(host.clone())
        .build()
        .await
        .unwrap();
    let outcome = execute(&factory).await;
    assert!(outcome.is_success(), "{outcome:?}");
    assert_eq!(host.calls.load(Ordering::SeqCst), 4);
    let trace = read_trace(&trace);
    let calls: Vec<_> = trace
        .iter()
        .filter(|request| request["method"] == "tools/call")
        .collect();
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0]["id"], calls[1]["id"]);
    for request in &calls {
        assert_eq!(
            request["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
            "2026-07-28"
        );
        assert_eq!(
            request["params"]["_meta"]["io.modelcontextprotocol/clientInfo"]["name"],
            "lash"
        );
    }
    assert_eq!(
        calls[0]["params"]["_meta"]["lash.dev/tool-call-id"],
        calls[1]["params"]["_meta"]["lash.dev/tool-call-id"]
    );
    assert_eq!(calls[1]["params"]["requestState"], "opaque-state");
    assert_eq!(
        calls[1]["params"]["inputResponses"]["url"],
        json!({"action":"accept"})
    );
    assert_eq!(
        calls[1]["params"]["inputResponses"]["form"]["content"],
        json!({"answer":"yes"})
    );
    assert_eq!(
        calls[1]["params"]["inputResponses"]["sampling"]["model"],
        "host-model"
    );
    assert_eq!(
        calls[1]["params"]["inputResponses"]["roots"]["roots"][0]["uri"],
        "file:///workspace/law"
    );
    assert!(
        !trace
            .iter()
            .any(|request| request["method"] == "initialize")
    );
    factory.shutdown().await.unwrap();
}

#[tokio::test]
async fn modern_form_answers_are_validated_before_retry() {
    let root = tempfile::tempdir().unwrap();
    let trace = root.path().join("trace.jsonl");
    let factory = McpPluginFactory::builder(configs(&trace, "invalid"))
        .elicitation_handler(Arc::new(InputHost::default()))
        .build()
        .await
        .unwrap();
    let result = execute(&factory).await;
    let lash_core::ToolCallOutcome::Failure(failure) = &result.as_done_output().unwrap().outcome
    else {
        panic!("invalid answer must fail")
    };
    assert_eq!(
        failure.raw.as_ref().unwrap().to_json_value()["error"]["data"]["kind"],
        "elicitation_response_validation"
    );
    assert_eq!(
        read_trace(&trace)
            .iter()
            .filter(|request| request["method"] == "tools/call")
            .count(),
        1
    );
    factory.shutdown().await.unwrap();
}

#[tokio::test]
async fn modern_tool_changes_use_acknowledged_subscription() {
    let root = tempfile::tempdir().unwrap();
    let trace = root.path().join("trace.jsonl");
    let factory = McpPluginFactory::new(configs(&trace, "changes"))
        .await
        .unwrap();
    let outcome = execute(&factory).await;
    assert!(outcome.is_success(), "{outcome:?}");
    tokio::time::timeout(Duration::from_secs(5), async {
        while factory.pool().advertised_tools()[0].manifest.description != "refreshed" {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let trace = read_trace(&trace);
    let listen = trace
        .iter()
        .find(|request| request["method"] == "subscriptions/listen")
        .unwrap();
    assert_eq!(
        listen["params"]["notifications"],
        json!({"toolsListChanged":true})
    );
    assert_eq!(
        trace
            .iter()
            .filter(|request| request["method"] == "tools/list")
            .count(),
        2
    );
    factory.shutdown().await.unwrap();
}

struct CancellingHost {
    started: tokio::sync::Notify,
    cancelled: tokio::sync::Notify,
}
#[async_trait]
impl McpElicitationHandler for CancellingHost {
    fn modes(&self) -> McpElicitationModes {
        McpElicitationModes::Form
    }
    async fn create_elicitation(
        &self,
        request: McpElicitationRequest<'_>,
    ) -> Result<McpElicitationResult, McpHostRefusal> {
        self.started.notify_one();
        request.context.cancellation_token().cancelled().await;
        self.cancelled.notify_one();
        Err(McpHostRefusal::internal("host input cancelled"))
    }
}

#[tokio::test]
async fn modern_mrtr_cancellation_reaches_the_host_without_retry() {
    let root = tempfile::tempdir().unwrap();
    let trace = root.path().join("trace.jsonl");
    let host = Arc::new(CancellingHost {
        started: tokio::sync::Notify::new(),
        cancelled: tokio::sync::Notify::new(),
    });
    let factory = McpPluginFactory::builder(configs(&trace, "cancel"))
        .elicitation_handler(host.clone())
        .build()
        .await
        .unwrap();
    let token = tokio_util::sync::CancellationToken::new();
    let fixture =
        lash_core::testing::ToolCallFixture::mock().cancellation_token(Some(token.clone()));
    let context = fixture.attempt("mrtr-cancel");
    let name = crate::mcp_tool_names("modern", &["exercise"])["exercise"].clone();
    let args = json!({});
    let call = factory.pool().call_tool(&name, &args, &context);
    tokio::pin!(call);
    tokio::select! {
        _ = host.started.notified() => {},
        outcome = &mut call => panic!("host must be waiting: {outcome:?}"),
    }
    token.cancel();
    let outcome = call.await;
    assert!(matches!(
        outcome.as_done_output().unwrap().outcome,
        lash_core::ToolCallOutcome::Cancelled(_)
    ));
    tokio::time::timeout(Duration::from_secs(5), host.cancelled.notified())
        .await
        .unwrap();
    assert_eq!(
        read_trace(&trace)
            .iter()
            .filter(|request| request["method"] == "tools/call")
            .count(),
        1
    );
    factory.shutdown().await.unwrap();
}

/// Answers each form after a short wait, keeping every request's
/// cancellation token for the law.
#[derive(Default)]
struct WatchedHost {
    tokens: std::sync::Mutex<Vec<tokio_util::sync::CancellationToken>>,
}
#[async_trait]
impl McpElicitationHandler for WatchedHost {
    fn modes(&self) -> McpElicitationModes {
        McpElicitationModes::Form
    }
    async fn create_elicitation(
        &self,
        request: McpElicitationRequest<'_>,
    ) -> Result<McpElicitationResult, McpHostRefusal> {
        let token = request.context.cancellation_token().clone();
        self.tokens.lock().unwrap().push(token.clone());
        tokio::select! {
            () = token.cancelled() => Err(McpHostRefusal::internal("host input cancelled")),
            () = tokio::time::sleep(Duration::from_millis(20)) => {
                request.accept(json!({"answer":"yes"})).map_err(McpHostRefusal::from)
            }
        }
    }
}

/// Ownership at the claim: a tool call owns the cancellation of the host
/// input it starts from the moment it creates it, so a call dropped at each
/// await, while the host is still answering among them, cancels every host
/// request it started instead of leaving it running unowned.
#[tokio::test]
async fn a_tool_call_dropped_at_each_await_cancels_the_host_input_it_started() {
    let root = tempfile::tempdir().unwrap();
    let trace = root.path().join("trace.jsonl");
    let host = Arc::new(WatchedHost::default());
    let factory = McpPluginFactory::builder(configs(&trace, "form"))
        .elicitation_handler(host.clone())
        .build()
        .await
        .unwrap();
    let name = crate::mcp_tool_names("modern", &["exercise"])["exercise"].clone();
    lash_core::cancel_sweep::sweep_drops(
        async || {
            let (pool, name) = (Arc::clone(factory.pool()), name.clone());
            let call = async move {
                let fixture = lash_core::testing::ToolCallFixture::mock();
                let context = fixture.attempt("swept-call");
                let outcome = pool.call_tool(&name, &json!({}), &context).await;
                assert!(outcome.is_success(), "the call completes: {outcome:?}");
            };
            ((), call)
        },
        async |(), cut| {
            let tokens = std::mem::take(&mut *host.tokens.lock().unwrap());
            assert!(
                tokens
                    .iter()
                    .all(tokio_util::sync::CancellationToken::is_cancelled),
                "every host input the call started is cancelled once the call is {cut}"
            );
        },
    )
    .await;
    factory.shutdown().await.unwrap();
}

async fn execute(factory: &McpPluginFactory) -> lash_core::ToolOutcome {
    use lash_core::ToolProvider;
    let provider = McpToolProvider::new(Arc::clone(factory.pool()));
    let mut manifests = provider
        .tool_manifests(&lash_core::OwnerConfigView::default())
        .unwrap();
    assert!(
        !manifests.is_empty(),
        "{:?}",
        factory.pool().server_statuses()
    );
    let manifest = manifests.remove(0);
    let prepared = crate::plugin::prepared_test_call(&provider, &manifest).await;
    let context = prepared.attempt("modern-input");
    let lash_core::ToolAttemptOutcome::Done { result, .. } = provider
        .execute(lash_core::ToolCall::new(
            &manifest,
            &lash_vocabulary::tool_contract::ToolContract::default(),
            &json!({}),
            &context,
        ))
        .await
    else {
        panic!("inline attempt")
    };
    lash_core::ToolOutcome::from_output(result.into_output())
}

fn configs(trace: &std::path::Path, scenario: &str) -> BTreeMap<String, McpServerConfig> {
    BTreeMap::from([(
        "modern".into(),
        McpServerConfig {
            events: Default::default(),
            startup_timeout_ms: 5_000,
            call_policy: crate::McpCallPolicy {
                call_timeout_ms: 5_000,
                reconnect_max_attempts: crate::ReconnectAttempts::Disabled,
                ..Default::default()
            },
            transport: McpTransport::Stdio(McpStdioTransport {
                command: "python3".into(),
                args: vec!["-u".into(), "-c".into(), SERVER.into()],
                env: BTreeMap::from([
                    ("TRACE".into(), trace.display().to_string()),
                    ("SCENARIO".into(), scenario.into()),
                ]),
                cwd: None,
            }),
            diagnostics: Default::default(),
            data_limits: Default::default(),
            shutdown_policy: Default::default(),
            trace_propagation: Default::default(),
        },
    )])
}
#[expect(
    clippy::disallowed_methods,
    reason = "test-only law reads the explicitly supplied protocol trace written by its own fixture"
)]
fn read_trace(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
const SERVER: &str = r#"
import json, os, sys
scenario=os.environ['SCENARIO']
subscription=None
lists=0
def send(message):
    print(json.dumps(message),flush=True)
def complete(message,result):
    send({'jsonrpc':'2.0','id':message['id'],'result':{'resultType':'complete',**result}})
for line in sys.stdin:
    with open(os.environ['TRACE'],'a') as trace: trace.write(line)
    message=json.loads(line)
    method=message.get('method')
    if method=='server/discover':
        complete(message,{'supportedVersions':[os.environ.get('REVISION','2026-07-28')], 'capabilities':{'tools':{'listChanged':scenario=='changes'}},'serverInfo':{'name':'modern-fixture','version':'1'},'ttlMs':0,'cacheScope':'private'})
    elif method=='tools/list':
        lists+=1
        complete(message,{'tools':[{'name':'exercise','description':'refreshed' if lists>1 else 'initial','inputSchema':{'type':'object'}}],'ttlMs':0,'cacheScope':'private'})
    elif method=='subscriptions/listen':
        subscription=message['id']
        send({'jsonrpc':'2.0','method':'notifications/subscriptions/acknowledged','params':{'notifications':message['params']['notifications'],'_meta':{'io.modelcontextprotocol/subscriptionId':subscription}}})
    elif method=='tools/call':
        if scenario=='changes':
            send({'jsonrpc':'2.0','method':'notifications/tools/list_changed','params':{'_meta':{'io.modelcontextprotocol/subscriptionId':subscription}}})
            complete(message,{'content':[{'type':'text','text':'changed'}]})
        elif 'inputResponses' in message.get('params',{}):
            complete(message,{'content':[{'type':'text','text':'complete'}]})
        else:
            form={'method':'elicitation/create','params':{'mode':'form','message':'invalid' if scenario=='invalid' else 'confirm','requestedSchema':{'type':'object','properties':{'answer':{'type':'string'}},'required':['answer']}}}
            requests={'form':form}
            if scenario=='inputs':
                # MCP 2026-07-28 still defaults an omitted elicitation mode to form.
                del form['params']['mode']
                requests.update({'url':{'method':'elicitation/create','params':{'mode':'url','message':'authorize','url':'https://example.invalid/approve'}},'sampling':{'method':'sampling/createMessage','params':{'messages':[{'role':'user','content':{'type':'text','text':'summarize'}}],'maxTokens':64}},'roots':{'method':'roots/list'}})
            send({'jsonrpc':'2.0','id':message['id'],'result':{'resultType':'input_required','requestState':'opaque-state','inputRequests':requests}})
"#;
