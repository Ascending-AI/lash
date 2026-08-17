use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use lash_core::ToolProvider as _;
use lash_core::plugin::PluginFactory as _;
use rmcp::model::{
    CreateElicitationRequestParams, CreateElicitationResult, CreateMessageResult,
    ElicitationAction, ElicitationCapability, ErrorData, FormElicitationCapability, Root,
    SamplingMessage, UrlElicitationCapability,
};
use serde_json::{Value, json};

use crate::{
    McpCallPolicy, McpElicitationHandler, McpElicitationRequest, McpPluginFactory,
    McpRootsProvider, McpRootsRequest, McpSamplingHandler, McpSamplingRequest, McpServerConfig,
    McpToolProvider, McpUrlElicitationComplete,
};

const SCRIPTED_SERVER: &str = r#"
import json
import os
import sys

trace_path = os.environ["TRACE_FILE"]
scenario = os.environ.get("SCENARIO", "round_trip")

def receive():
    line = sys.stdin.readline()
    if not line:
        raise SystemExit(0)
    return json.loads(line)

def send(message):
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def record(kind, message):
    with open(trace_path, "a", encoding="utf-8") as trace:
        trace.write(json.dumps({"kind": kind, "message": message}, separators=(",", ":")) + "\n")

initialize = receive()
record("initialize", initialize)
send({
    "jsonrpc": "2.0",
    "id": initialize["id"],
    "result": {
        "protocolVersion": initialize["params"]["protocolVersion"],
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "client-depth-script", "version": "1.0.0"},
    },
})
record("initialized", receive())
list_tools = receive()
record("tools_list", list_tools)
send({
    "jsonrpc": "2.0",
    "id": list_tools["id"],
    "result": {
        "tools": [{
            "name": "exercise-client-depth",
            "description": "Exercise sampling, elicitation, and roots",
            "inputSchema": {"type": "object", "properties": {}},
        }]
    },
})

next_message = receive()
if next_message.get("method") != "tools/call":
    record("extra", next_message)
    for line in sys.stdin:
        record("extra", json.loads(line))
    raise SystemExit(0)

record("tools_call", next_message)

form_request = {
    "mode": "form",
    "message": "May the server continue?",
    "requestedSchema": {
        "type": "object",
        "properties": {
            "answer": {"type": "string"},
            "contact": {"type": "string", "format": "email"},
        },
        "required": ["answer", "contact"],
    },
}

if scenario == "unwired":
    requests = [
        ("sampling_response", {
            "jsonrpc": "2.0",
            "id": 100,
            "method": "sampling/createMessage",
            "params": {
                "messages": [{"role": "user", "content": {"type": "text", "text": "test"}}],
                "maxTokens": 64,
            },
        }),
        ("elicitation_response", {
            "jsonrpc": "2.0",
            "id": 101,
            "method": "elicitation/create",
            "params": form_request,
        }),
        ("roots_response", {"jsonrpc": "2.0", "id": 102, "method": "roots/list"}),
    ]
    for kind, request in requests:
        send(request)
        record(kind, receive())
elif scenario == "cancel":
    send({
        "jsonrpc": "2.0",
        "id": 101,
        "method": "elicitation/create",
        "params": form_request,
    })
    send({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {"requestId": 101, "reason": "test cancellation"},
    })
    record("elicitation_response", receive())
elif scenario == "schema":
    for request_id, kind in [
        (101, "conforming_response"),
        (102, "wrong_type_response"),
        (103, "missing_required_response"),
        (104, "invalid_email_response"),
    ]:
        send({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "elicitation/create",
            "params": form_request,
        })
        record(kind, receive())
elif scenario == "url":
    send({
        "jsonrpc": "2.0",
        "id": 101,
        "method": "elicitation/create",
        "params": {
            "mode": "url",
            "message": "Complete authorization in the browser",
            "url": "https://example.invalid/authorize",
            "elicitationId": "url-flow-17",
        },
    })
    record("url_response", receive())
    send({
        "jsonrpc": "2.0",
        "method": "notifications/elicitation/complete",
        "params": {"elicitationId": "url-flow-17"},
    })
else:
    send({
        "jsonrpc": "2.0",
        "id": 100,
        "method": "sampling/createMessage",
        "params": {
            "messages": [{"role": "user", "content": {"type": "text", "text": "Summarize: Lash keeps policy with the host."}}],
            "maxTokens": 64,
        },
    })
    record("sampling_response", receive())

    send({
        "jsonrpc": "2.0",
        "id": 101,
        "method": "elicitation/create",
        "params": form_request,
    })
    record("elicitation_response", receive())

    send({"jsonrpc": "2.0", "id": 102, "method": "roots/list"})
    record("roots_response", receive())

send({
    "jsonrpc": "2.0",
    "id": next_message["id"],
    "result": {
        "content": [{"type": "text", "text": "client depth complete"}],
        "structuredContent": {"complete": True},
    },
})

for line in sys.stdin:
    message = json.loads(line)
    record("after_call", message)
"#;

struct SamplingHost {
    calls: AtomicUsize,
}

#[async_trait]
impl McpSamplingHandler for SamplingHost {
    async fn create_message(
        &self,
        request: McpSamplingRequest<'_>,
    ) -> Result<CreateMessageResult, ErrorData> {
        assert_eq!(request.context.server_name(), "depth");
        assert_eq!(request.params.max_tokens, 64);
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CreateMessageResult::new(
            SamplingMessage::assistant_text("Policy stays with the host."),
            "host-demo-model".to_string(),
        )
        .with_stop_reason(CreateMessageResult::STOP_REASON_END_TURN))
    }
}

struct DecliningElicitationHost {
    calls: AtomicUsize,
}

#[async_trait]
impl McpElicitationHandler for DecliningElicitationHost {
    fn capability(&self) -> ElicitationCapability {
        ElicitationCapability {
            form: Some(FormElicitationCapability::default()),
            url: None,
        }
    }

    async fn create_elicitation(
        &self,
        request: McpElicitationRequest<'_>,
    ) -> Result<CreateElicitationResult, ErrorData> {
        assert_eq!(request.context.server_name(), "depth");
        assert!(matches!(
            request.params,
            CreateElicitationRequestParams::FormElicitationParams { .. }
        ));
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CreateElicitationResult::new(ElicitationAction::Decline))
    }

    async fn url_elicitation_complete(&self, _notification: McpUrlElicitationComplete<'_>) {
        unreachable!("form-only test host cannot receive URL completion")
    }
}

struct WorkspaceRoots {
    calls: AtomicUsize,
}

#[async_trait]
impl McpRootsProvider for WorkspaceRoots {
    async fn list_roots(&self, request: McpRootsRequest<'_>) -> Result<Vec<Root>, ErrorData> {
        assert_eq!(request.context.server_name(), "depth");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![Root::new("file:///workspace/demo").with_name("demo")])
    }
}

struct CancellationObservingHost {
    cancelled: AtomicBool,
}

#[async_trait]
impl McpElicitationHandler for CancellationObservingHost {
    fn capability(&self) -> ElicitationCapability {
        ElicitationCapability {
            form: Some(FormElicitationCapability::default()),
            url: None,
        }
    }

    async fn create_elicitation(
        &self,
        request: McpElicitationRequest<'_>,
    ) -> Result<CreateElicitationResult, ErrorData> {
        request.context.cancellation_token().cancelled().await;
        self.cancelled.store(true, Ordering::SeqCst);
        Err(ErrorData::internal_error(
            "host stopped the cancelled elicitation",
            None,
        ))
    }

    async fn url_elicitation_complete(&self, _notification: McpUrlElicitationComplete<'_>) {
        unreachable!("form-only test host cannot receive URL completion")
    }
}

struct SchemaCheckingHost {
    calls: AtomicUsize,
}

#[async_trait]
impl McpElicitationHandler for SchemaCheckingHost {
    fn capability(&self) -> ElicitationCapability {
        ElicitationCapability {
            form: Some(FormElicitationCapability::default()),
            url: None,
        }
    }

    async fn create_elicitation(
        &self,
        request: McpElicitationRequest<'_>,
    ) -> Result<CreateElicitationResult, ErrorData> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let content = match call {
            0 => json!({ "answer": "yes", "contact": "host@example.com" }),
            1 => json!({ "answer": true, "contact": "host@example.com" }),
            2 => json!({}),
            3 => json!({ "answer": "yes", "contact": "not-an-email" }),
            other => panic!("unexpected schema test call {other}"),
        };
        match call {
            0 => request
                .accept(content)
                .map_err(|error| ErrorData::invalid_params(error.to_string(), None)),
            1..=3 => {
                let host_error = request
                    .accept(content.clone())
                    .expect_err("host sees typed validation error before returning an answer");
                assert!(host_error.message().contains("requested schema"));
                Ok(CreateElicitationResult::new(ElicitationAction::Accept).with_content(content))
            }
            _ => unreachable!(),
        }
    }

    async fn url_elicitation_complete(&self, _notification: McpUrlElicitationComplete<'_>) {
        unreachable!("form-only test host cannot receive URL completion")
    }
}

struct UrlElicitationHost {
    completed: AtomicBool,
    capability_calls: AtomicUsize,
}

#[async_trait]
impl McpElicitationHandler for UrlElicitationHost {
    fn capability(&self) -> ElicitationCapability {
        self.capability_calls.fetch_add(1, Ordering::SeqCst);
        ElicitationCapability {
            form: None,
            url: Some(UrlElicitationCapability::default()),
        }
    }

    async fn create_elicitation(
        &self,
        request: McpElicitationRequest<'_>,
    ) -> Result<CreateElicitationResult, ErrorData> {
        assert_eq!(request.context.server_name(), "depth");
        assert!(matches!(
            request.params,
            CreateElicitationRequestParams::UrlElicitationParams {
                elicitation_id,
                ..
            } if elicitation_id == "url-flow-17"
        ));
        Ok(CreateElicitationResult::new(ElicitationAction::Accept))
    }

    async fn url_elicitation_complete(&self, notification: McpUrlElicitationComplete<'_>) {
        assert_eq!(notification.context.server_name(), "depth");
        assert_eq!(notification.elicitation_id, "url-flow-17");
        self.completed.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn scripted_server_round_trips_sampling_elicitation_roots_and_change_notification() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let trace = scratch.path().join("protocol.jsonl");
    let sampling = Arc::new(SamplingHost {
        calls: AtomicUsize::new(0),
    });
    let elicitation = Arc::new(DecliningElicitationHost {
        calls: AtomicUsize::new(0),
    });
    let roots = Arc::new(WorkspaceRoots {
        calls: AtomicUsize::new(0),
    });
    let factory = McpPluginFactory::builder(scripted_servers(&trace, "round_trip"))
        .sampling_handler(sampling.clone())
        .elicitation_handler(elicitation.clone())
        .roots_provider(roots.clone())
        .build()
        .await
        .expect("connect scripted MCP server");

    let provider = McpToolProvider::new(Arc::clone(factory.pool()));
    let result = provider
        .execute(lash_core::ToolCall {
            name: "mcp__depth__exercise_client_depth",
            args: &json!({}),
            context: &lash_core::testing::mock_attempt_context(),
        })
        .await;
    assert!(
        result.is_success(),
        "nested client requests complete: {result:?}"
    );
    assert_eq!(sampling.calls.load(Ordering::SeqCst), 1);
    assert_eq!(elicitation.calls.load(Ordering::SeqCst), 1);
    assert_eq!(roots.calls.load(Ordering::SeqCst), 1);

    factory
        .notify_roots_changed()
        .await
        .expect("broadcast roots/list_changed");
    wait_for_trace(&trace, |events| {
        events.iter().any(|event| {
            event["kind"] == "after_call"
                && event["message"]["method"] == "notifications/roots/list_changed"
        })
    })
    .await;

    let events = read_trace(&trace);
    let initialize = event(&events, "initialize");
    assert_eq!(initialize["params"]["protocolVersion"], "2025-11-25");
    assert_eq!(initialize["params"]["capabilities"]["sampling"], json!({}));
    assert_eq!(
        initialize["params"]["capabilities"]["elicitation"],
        json!({ "form": {} })
    );
    assert_eq!(
        initialize["params"]["capabilities"]["roots"],
        json!({ "listChanged": true })
    );

    let sampling_response = event(&events, "sampling_response");
    assert_eq!(sampling_response["result"]["model"], "host-demo-model");
    assert_eq!(
        sampling_response["result"]["content"]["text"],
        "Policy stays with the host."
    );
    let elicitation_response = event(&events, "elicitation_response");
    assert_eq!(
        elicitation_response["result"],
        json!({ "action": "decline" }),
        "decline is a typed MCP result and must never be rewritten as accept"
    );
    let roots_response = event(&events, "roots_response");
    assert_eq!(
        roots_response["result"]["roots"],
        json!([{ "uri": "file:///workspace/demo", "name": "demo" }])
    );

    factory.shutdown().await.expect("shutdown MCP factory");
}

#[tokio::test]
async fn scripted_handshake_omits_every_unwired_client_capability() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let trace = scratch.path().join("protocol.jsonl");
    let factory = McpPluginFactory::new(scripted_servers(&trace, "round_trip"))
        .await
        .expect("connect scripted MCP server");

    let events = read_trace(&trace);
    let capabilities = &event(&events, "initialize")["params"]["capabilities"];
    assert!(capabilities.get("sampling").is_none());
    assert!(capabilities.get("elicitation").is_none());
    assert!(capabilities.get("roots").is_none());
    assert!(
        factory.notify_roots_changed().await.is_err(),
        "roots notifications require the roots seam"
    );

    factory.shutdown().await.expect("shutdown MCP factory");
}

#[tokio::test]
async fn unwired_requests_return_typed_method_errors_without_failing_the_outer_tool_call() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let trace = scratch.path().join("protocol.jsonl");
    let factory = McpPluginFactory::new(scripted_servers(&trace, "unwired"))
        .await
        .expect("connect scripted MCP server");

    let result = execute_depth_tool(&factory).await;
    assert!(result.is_success(), "outer tool call survives: {result:?}");

    let events = read_trace(&trace);
    for (kind, message) in [
        (
            "sampling_response",
            "sampling is not available: no host sampling handler is installed",
        ),
        (
            "elicitation_response",
            "elicitation is not available: no host elicitation handler is installed",
        ),
        (
            "roots_response",
            "roots is not available: no host roots handler is installed",
        ),
    ] {
        let response = event(&events, kind);
        assert_eq!(response["error"]["code"], -32601);
        assert_eq!(response["error"]["message"], message);
    }

    factory.shutdown().await.expect("shutdown MCP factory");
}

#[tokio::test]
async fn cancelled_elicitation_reaches_the_host_token_without_an_orphaned_prompt() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let trace = scratch.path().join("protocol.jsonl");
    let elicitation = Arc::new(CancellationObservingHost {
        cancelled: AtomicBool::new(false),
    });
    let factory = McpPluginFactory::builder(scripted_servers(&trace, "cancel"))
        .elicitation_handler(elicitation.clone())
        .build()
        .await
        .expect("connect scripted MCP server");

    let result = execute_depth_tool(&factory).await;
    assert!(result.is_success(), "outer tool call survives: {result:?}");
    assert!(
        elicitation.cancelled.load(Ordering::SeqCst),
        "host observed the request cancellation token"
    );
    assert_eq!(
        event(&read_trace(&trace), "elicitation_response")["error"]["message"],
        "host stopped the cancelled elicitation"
    );

    factory.shutdown().await.expect("shutdown MCP factory");
}

#[tokio::test]
async fn form_answers_are_validated_before_they_cross_the_wire() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let trace = scratch.path().join("protocol.jsonl");
    let elicitation = Arc::new(SchemaCheckingHost {
        calls: AtomicUsize::new(0),
    });
    let factory = McpPluginFactory::builder(scripted_servers(&trace, "schema"))
        .elicitation_handler(elicitation.clone())
        .build()
        .await
        .expect("connect scripted MCP server");

    let result = execute_depth_tool(&factory).await;
    assert!(result.is_success(), "outer tool call survives: {result:?}");
    assert_eq!(elicitation.calls.load(Ordering::SeqCst), 4);

    let events = read_trace(&trace);
    assert_eq!(
        event(&events, "conforming_response")["result"],
        json!({
            "action": "accept",
            "content": { "answer": "yes", "contact": "host@example.com" }
        })
    );
    for kind in [
        "wrong_type_response",
        "missing_required_response",
        "invalid_email_response",
    ] {
        let response = event(&events, kind);
        assert_eq!(response["error"]["code"], -32602);
        assert_eq!(
            response["error"]["data"]["kind"],
            "elicitation_response_validation"
        );
        assert!(
            response.get("result").is_none(),
            "malformed content was not sent"
        );
    }

    factory.shutdown().await.expect("shutdown MCP factory");
}

#[tokio::test]
async fn advertised_url_elicitation_routes_its_completion_notification() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let trace = scratch.path().join("protocol.jsonl");
    let elicitation = Arc::new(UrlElicitationHost {
        completed: AtomicBool::new(false),
        capability_calls: AtomicUsize::new(0),
    });
    let factory = McpPluginFactory::builder(scripted_servers(&trace, "url"))
        .elicitation_handler(elicitation.clone())
        .build()
        .await
        .expect("connect scripted MCP server");

    let events = read_trace(&trace);
    let capabilities = &event(&events, "initialize")["params"]["capabilities"];
    assert_eq!(capabilities["elicitation"], json!({ "url": {} }));
    let result = execute_depth_tool(&factory).await;
    assert!(
        result.is_success(),
        "URL flow outer call succeeds: {result:?}"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut interval = tokio::time::interval(Duration::from_millis(10));
        while !elicitation.completed.load(Ordering::SeqCst) {
            interval.tick().await;
        }
    })
    .await
    .expect("URL completion hook was not called");
    assert!(elicitation.completed.load(Ordering::SeqCst));
    assert_eq!(
        elicitation.capability_calls.load(Ordering::SeqCst),
        1,
        "elicitation capability is snapshotted once at build"
    );
    assert_eq!(
        event(&read_trace(&trace), "url_response")["result"],
        json!({ "action": "accept" })
    );

    factory.shutdown().await.expect("shutdown MCP factory");
}

async fn execute_depth_tool(factory: &McpPluginFactory) -> lash_core::ToolResult {
    McpToolProvider::new(Arc::clone(factory.pool()))
        .execute(lash_core::ToolCall {
            name: "mcp__depth__exercise_client_depth",
            args: &json!({}),
            context: &lash_core::testing::mock_attempt_context(),
        })
        .await
}

fn scripted_servers(trace: &std::path::Path, scenario: &str) -> BTreeMap<String, McpServerConfig> {
    BTreeMap::from([(
        "depth".to_string(),
        McpServerConfig::Stdio {
            command: "python3".to_string(),
            args: vec![
                "-u".to_string(),
                "-c".to_string(),
                SCRIPTED_SERVER.to_string(),
            ],
            env: BTreeMap::from([
                ("TRACE_FILE".to_string(), trace.display().to_string()),
                ("SCENARIO".to_string(), scenario.to_string()),
            ]),
            cwd: None,
            startup_timeout_ms: 10_000,
            call_policy: McpCallPolicy {
                call_timeout_ms: 10_000,
                ..Default::default()
            },
            binary_content_attachments: false,
        },
    )])
}

fn read_trace(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("read protocol trace")
        .lines()
        .map(|line| serde_json::from_str(line).expect("decode protocol trace line"))
        .collect()
}

fn event<'a>(events: &'a [Value], kind: &str) -> &'a Value {
    &events
        .iter()
        .find(|event| event["kind"] == kind)
        .unwrap_or_else(|| panic!("missing `{kind}` in {events:?}"))["message"]
}

async fn wait_for_trace(path: &std::path::Path, predicate: impl Fn(&[Value]) -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let events = read_trace(path);
            if predicate(&events) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("scripted server did not record expected protocol message");
}
