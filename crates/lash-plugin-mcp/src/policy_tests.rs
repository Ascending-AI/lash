use lash_sansio::sync::{MutexExt, RwLockExt};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core::{ToolCallOutcome, ToolFailure, ToolFailureClass, ToolResult};
use rmcp::model::{
    CancelledNotification, CancelledNotificationParam, ClientNotification, RequestId,
};
use rmcp::service::{Peer, RoleClient};
use serde_json::json;

use super::*;

const MOCK_SERVER: &str = r#"
import json, os, sys, threading, time

lock = threading.Lock()
behavior = os.environ['BEHAVIOR']
protocol = os.environ.get('PROTOCOL', '2025-11-25')
log_path = os.environ['LOG_PATH']
starts_path = os.environ['STARTS_PATH']

try:
    with open(starts_path, 'r', encoding='utf-8') as f:
        starts = int(f.read())
except (FileNotFoundError, ValueError):
    starts = 0
with open(starts_path, 'w', encoding='utf-8') as f:
    f.write(str(starts + 1))
if behavior == 'silent_no_ping_once' and starts > 0:
    sys.exit(1)
if behavior == 'fail_twice_then_success' and starts < 2:
    sys.exit(1)

def send(message):
    with lock:
        sys.stdout.write(json.dumps(message, separators=(',', ':')) + '\n')
        sys.stdout.flush()

def result(request_id):
    send({'jsonrpc': '2.0', 'id': request_id,
          'result': {'content': [{'type': 'text', 'text': 'ok'}]}})

def run_call(message, index):
    request_id = message['id']
    token = message.get('params', {}).get('_meta', {}).get('progressToken')
    if behavior == 'progress':
        for step in range(5):
            time.sleep(0.1)
            send({'jsonrpc': '2.0', 'method': 'notifications/progress',
                  'params': {'progressToken': token, 'progress': step + 1}})
        result(request_id)
    elif behavior == 'continuous_progress':
        for step in range(20):
            time.sleep(0.1)
            send({'jsonrpc': '2.0', 'method': 'notifications/progress',
                  'params': {'progressToken': token, 'progress': step + 1}})
        result(request_id)
    elif behavior == 'sequence':
        if index == 2:
            result(request_id)
    elif behavior == 'success':
        result(request_id)

call_index = 0
for line in sys.stdin:
    with open(log_path, 'a', encoding='utf-8') as log:
        log.write(line)
    message = json.loads(line)
    method = message.get('method')
    if method == 'initialize':
        send({'jsonrpc': '2.0', 'id': message['id'], 'result': {
            'protocolVersion': protocol,
            'capabilities': {'tools': {}},
            'serverInfo': {'name': 'policy-mock', 'version': '1.0.0'}}})
    elif method == 'tools/list':
        send({'jsonrpc': '2.0', 'id': message['id'], 'result': {'tools': [{
            'name': 'work', 'description': 'Policy test tool',
            'inputSchema': {'type': 'object', 'properties': {}}}]}})
        if behavior == 'exit_after_list':
            sys.exit(0)
    elif method == 'tools/call':
        call_index += 1
        threading.Thread(target=run_call, args=(message, call_index), daemon=True).start()
    elif method == 'ping':
        if behavior == 'ping_error':
            send({'jsonrpc': '2.0', 'id': message['id'],
                  'error': {'code': -32601, 'message': 'Method not found'}})
        elif behavior == 'ping_meta':
            send({'jsonrpc': '2.0', 'id': message['id'],
                  'result': {'_meta': {'alive': True}}})
        elif behavior in ('silent_ping', 'success', 'progress', 'continuous_progress', 'sequence', 'fail_twice_then_success'):
            send({'jsonrpc': '2.0', 'id': message['id'], 'result': {}})
"#;

#[derive(Clone, Copy)]
struct MockOptions {
    behavior: &'static str,
    protocol: &'static str,
    call_timeout_ms: u64,
    call_max_total_timeout_ms: u64,
    reset_on_progress: bool,
    policy: TimeoutDisconnectPolicy,
    probe_timeout_ms: u64,
    threshold: u64,
    probe_interval_ms: u64,
    reconnect_initial_ms: u64,
    reconnect_max_attempts: u64,
}

impl Default for MockOptions {
    fn default() -> Self {
        Self {
            behavior: "silent",
            protocol: "2025-11-25",
            call_timeout_ms: 150,
            call_max_total_timeout_ms: 2_000,
            reset_on_progress: true,
            policy: TimeoutDisconnectPolicy::Never,
            probe_timeout_ms: 100,
            threshold: 3,
            probe_interval_ms: 0,
            reconnect_initial_ms: 5_000,
            reconnect_max_attempts: 1,
        }
    }
}

fn mock_config(root: &Path, options: MockOptions) -> McpServerConfig {
    McpServerConfig::Stdio {
        command: "python3".to_string(),
        args: vec!["-u".to_string(), "-c".to_string(), MOCK_SERVER.to_string()],
        env: BTreeMap::from([
            ("BEHAVIOR".to_string(), options.behavior.to_string()),
            ("PROTOCOL".to_string(), options.protocol.to_string()),
            (
                "LOG_PATH".to_string(),
                root.join("received.jsonl").display().to_string(),
            ),
            (
                "STARTS_PATH".to_string(),
                root.join("starts").display().to_string(),
            ),
        ]),
        cwd: None,
        startup_timeout_ms: 1_000,
        call_policy: McpCallPolicy {
            call_timeout_ms: options.call_timeout_ms,
            call_max_total_timeout_ms: options.call_max_total_timeout_ms,
            reset_call_timeout_on_progress: options.reset_on_progress,
            timeout_disconnect_policy: options.policy,
            liveness_probe_timeout_ms: options.probe_timeout_ms,
            consecutive_timeouts_before_disconnect: options.threshold,
            liveness_probe_interval_ms: options.probe_interval_ms,
            reconnect_initial_backoff_ms: options.reconnect_initial_ms,
            reconnect_max_backoff_ms: options.reconnect_initial_ms,
            reconnect_max_attempts: options.reconnect_max_attempts,
        },
        binary_content_attachments: false,
    }
}

async fn connect_mock(root: &Path, options: MockOptions) -> Arc<McpConnectionPool> {
    McpConnectionPool::connect(BTreeMap::from([(
        "mock".to_string(),
        mock_config(root, options),
    )]))
    .await
    .expect("connect policy mock")
}

async fn call(pool: &McpConnectionPool) -> ToolResult {
    pool.call_tool(
        "mcp__mock__work",
        &json!({}),
        &lash_core::testing::mock_tool_context(),
    )
    .await
}

fn failure(result: &ToolResult) -> &ToolFailure {
    let output = result.as_done_output().expect("completed tool result");
    let ToolCallOutcome::Failure(failure) = &output.outcome else {
        panic!("expected tool failure, got {output:?}");
    };
    failure
}

fn received(root: &Path) -> String {
    std::fs::read_to_string(root.join("received.jsonl")).unwrap_or_default()
}

fn starts(root: &Path) -> u64 {
    std::fs::read_to_string(root.join("starts"))
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

async fn wait_until(mut condition: impl FnMut() -> bool, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < deadline, "{message}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn peer(pool: &McpConnectionPool) -> Peer<RoleClient> {
    entry(pool)
        .service
        .lock()
        .await
        .as_ref()
        .expect("connected service")
        .peer()
        .clone()
}

fn entry(pool: &McpConnectionPool) -> Arc<McpEntry> {
    pool.entries
        .read_recover()
        .get("mock")
        .expect("mock entry")
        .clone()
}

fn request_id_for_method(root: &Path, method: &str) -> Option<RequestId> {
    received(root).lines().find_map(|line| {
        let message: serde_json::Value = serde_json::from_str(line).ok()?;
        (message.get("method")?.as_str()? == method)
            .then(|| serde_json::from_value(message.get("id")?.clone()).ok())?
    })
}

#[tokio::test]
async fn progress_resets_idle_timeout_and_allows_three_times_the_idle_budget() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "progress",
            call_timeout_ms: 150,
            ..MockOptions::default()
        },
    )
    .await;

    let started = Instant::now();
    let result = call(&pool).await;
    assert!(result.is_success(), "progressing call failed: {result:?}");
    assert!(started.elapsed() >= Duration::from_millis(450));
    pool.shutdown_all().await;
}

#[tokio::test]
async fn progress_does_not_reset_idle_timeout_when_disabled() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "progress",
            reset_on_progress: false,
            ..MockOptions::default()
        },
    )
    .await;

    let started = Instant::now();
    let result = call(&pool).await;
    assert_eq!(failure(&result).class, ToolFailureClass::Timeout);
    assert!(started.elapsed() >= Duration::from_millis(125));
    assert!(started.elapsed() < Duration::from_millis(350));
    pool.shutdown_all().await;
}

#[tokio::test]
async fn wall_clock_cap_fires_despite_continuous_progress() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "continuous_progress",
            call_timeout_ms: 150,
            call_max_total_timeout_ms: 350,
            policy: TimeoutDisconnectPolicy::ConsecutiveTimeouts,
            threshold: 1,
            ..MockOptions::default()
        },
    )
    .await;

    let started = Instant::now();
    let result = call(&pool).await;
    assert_eq!(failure(&result).class, ToolFailureClass::Timeout);
    assert_eq!(failure(&result).code, "mcp_call_deadline_exceeded");
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert!(started.elapsed() < Duration::from_millis(600));
    assert!(pool.server_statuses()[0].connected);
    assert_eq!(
        entry(&pool).consecutive_timeouts.load(Ordering::SeqCst),
        0,
        "wall-cap expiry must not consume the idle-timeout budget"
    );
    pool.shutdown_all().await;
}

#[tokio::test]
async fn idle_timeout_emits_cancellation_notification() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(root.path(), MockOptions::default()).await;

    let result = call(&pool).await;
    assert_eq!(failure(&result).class, ToolFailureClass::Timeout);
    wait_until(
        || received(root.path()).contains("notifications/cancelled"),
        "mock never received timeout cancellation notification",
    )
    .await;
    pool.shutdown_all().await;
}

#[tokio::test]
async fn silent_tool_with_answered_ping_times_out_without_disconnect() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "silent_ping",
            call_timeout_ms: 50,
            policy: TimeoutDisconnectPolicy::PingProbe,
            ..MockOptions::default()
        },
    )
    .await;

    let result = call(&pool).await;
    assert_eq!(failure(&result).class, ToolFailureClass::Timeout);
    assert!(pool.server_statuses()[0].connected);
    assert!(received(root.path()).contains("\"method\":\"ping\""));
    pool.shutdown_all().await;
}

#[tokio::test]
async fn ping_method_not_found_answer_proves_liveness() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "ping_error",
            call_timeout_ms: 50,
            policy: TimeoutDisconnectPolicy::PingProbe,
            ..MockOptions::default()
        },
    )
    .await;

    let result = call(&pool).await;
    assert_eq!(failure(&result).class, ToolFailureClass::Timeout);
    assert!(pool.server_statuses()[0].connected);
    pool.shutdown_all().await;
}

#[tokio::test]
async fn ping_meta_result_answer_proves_liveness() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "ping_meta",
            call_timeout_ms: 50,
            policy: TimeoutDisconnectPolicy::PingProbe,
            ..MockOptions::default()
        },
    )
    .await;

    let result = call(&pool).await;
    assert_eq!(failure(&result).class, ToolFailureClass::Timeout);
    assert!(pool.server_statuses()[0].connected);
    pool.shutdown_all().await;
}

#[tokio::test]
async fn silent_tool_and_failed_ping_disconnects_and_runs_one_reconnect_cycle() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "silent_no_ping_once",
            call_timeout_ms: 50,
            policy: TimeoutDisconnectPolicy::PingProbe,
            reconnect_initial_ms: 10,
            reconnect_max_attempts: 1,
            ..MockOptions::default()
        },
    )
    .await;

    let result = call(&pool).await;
    assert_eq!(failure(&result).class, ToolFailureClass::Unavailable);
    wait_until(|| starts(root.path()) == 2, "reconnect cycle did not run").await;
    let status = &pool.server_statuses()[0];
    assert!(!status.connected);
    assert!(status.last_error.is_some());
    assert_eq!(starts(root.path()), 2);
    pool.shutdown_all().await;
}

#[tokio::test]
async fn consecutive_timeout_threshold_resets_only_after_success() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "sequence",
            call_timeout_ms: 50,
            policy: TimeoutDisconnectPolicy::ConsecutiveTimeouts,
            threshold: 2,
            reconnect_initial_ms: 10,
            ..MockOptions::default()
        },
    )
    .await;

    assert_eq!(failure(&call(&pool).await).class, ToolFailureClass::Timeout);
    *entry(&pool).last_error.write_recover() = Some("stale error".to_string());
    assert!(call(&pool).await.is_success());
    assert!(pool.server_statuses()[0].last_error.is_none());
    assert_eq!(failure(&call(&pool).await).class, ToolFailureClass::Timeout);
    assert_eq!(
        failure(&call(&pool).await).class,
        ToolFailureClass::Unavailable
    );
    assert!(!pool.server_statuses()[0].connected);
    wait_until(
        || pool.server_statuses()[0].connected,
        "threshold disconnect did not reconnect",
    )
    .await;
    assert_eq!(failure(&call(&pool).await).class, ToolFailureClass::Timeout);
    assert!(
        pool.server_statuses()[0].connected,
        "the first timeout after reconnect must start a fresh budget"
    );
    pool.shutdown_all().await;
}

#[tokio::test]
async fn late_probe_failure_cannot_disconnect_a_replacement_service() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            call_timeout_ms: 50,
            probe_timeout_ms: 300,
            policy: TimeoutDisconnectPolicy::PingProbe,
            ..MockOptions::default()
        },
    )
    .await;
    let current_entry = entry(&pool);
    let call_task = tokio::spawn({
        let pool = Arc::clone(&pool);
        async move { call(&pool).await }
    });
    wait_until(
        || received(root.path()).contains("\"method\":\"ping\""),
        "probe was not dispatched",
    )
    .await;

    current_entry
        .try_connect()
        .await
        .expect("replacement connection");
    let result = call_task.await.expect("call task");
    assert_eq!(failure(&result).class, ToolFailureClass::Timeout);
    assert!(pool.server_statuses()[0].connected);
    assert_eq!(starts(root.path()), 2);
    pool.shutdown_all().await;
}

#[tokio::test]
async fn keepalive_rearms_an_exhausted_reconnect_loop() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "fail_twice_then_success",
            probe_interval_ms: 25,
            reconnect_initial_ms: 10,
            reconnect_max_attempts: 1,
            ..MockOptions::default()
        },
    )
    .await;

    wait_until(
        || starts(root.path()) >= 3 && pool.server_statuses()[0].connected,
        "keepalive did not re-arm exhausted reconnect attempts",
    )
    .await;
    pool.shutdown_all().await;
}

#[tokio::test]
async fn dropped_pool_releases_entry_and_keepalive_task() {
    let root = tempfile::tempdir().unwrap();
    let pool = Arc::new(McpConnectionPool::empty());
    let entry = Arc::new(McpEntry::new(
        "mock".to_string(),
        mock_config(
            root.path(),
            MockOptions {
                probe_interval_ms: 10,
                ..MockOptions::default()
            },
        ),
    ));
    let weak = Arc::downgrade(&entry);
    let keepalive = entry
        .spawn_keepalive_loop()
        .expect("enabled keepalive task");
    assert!(pool.install("mock".to_string(), Arc::clone(&entry)).is_ok());
    drop(entry);
    drop(pool);

    tokio::time::timeout(Duration::from_millis(100), keepalive)
        .await
        .expect("keepalive task retained the dropped entry")
        .expect("keepalive task panicked");
    assert!(weak.upgrade().is_none());
}

#[derive(Clone, Default)]
struct TraceBuffer(Arc<Mutex<Vec<u8>>>);

struct TraceWriter(Arc<Mutex<Vec<u8>>>);

impl Write for TraceWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock_recover().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for TraceBuffer {
    type Writer = TraceWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        TraceWriter(Arc::clone(&self.0))
    }
}

#[tokio::test]
async fn protocol_2026_degrades_ping_policy_to_counting_and_warns_once() {
    let traces = TraceBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(traces.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            protocol: "2026-07-28",
            call_timeout_ms: 50,
            policy: TimeoutDisconnectPolicy::PingProbe,
            threshold: 2,
            probe_interval_ms: 20,
            ..MockOptions::default()
        },
    )
    .await;

    assert_eq!(failure(&call(&pool).await).class, ToolFailureClass::Timeout);
    assert_eq!(
        failure(&call(&pool).await).class,
        ToolFailureClass::Unavailable
    );
    assert!(!received(root.path()).contains("\"method\":\"ping\""));
    let trace = String::from_utf8(traces.0.lock_recover().clone()).unwrap();
    let warning = "degrading timeout policy to consecutive_timeouts";
    assert_eq!(trace.matches(warning).count(), 1, "captured trace: {trace}");
    pool.shutdown_all().await;
}

#[tokio::test]
async fn cancelled_call_is_call_level_and_keeps_connection() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(root.path(), MockOptions::default()).await;
    let peer = peer(&pool).await;

    let call = call(&pool);
    let cancel = async {
        wait_until(
            || received(root.path()).contains("\"method\":\"tools/call\""),
            "tool call was not dispatched",
        )
        .await;
        peer.send_notification(ClientNotification::CancelledNotification(
            CancelledNotification::new(CancelledNotificationParam {
                request_id: request_id_for_method(root.path(), "tools/call")
                    .expect("tools/call request id"),
                reason: Some("test cancellation".to_string()),
            }),
        ))
        .await
        .expect("send cancellation");
    };
    let (result, ()) = tokio::join!(call, cancel);
    assert!(matches!(
        result.as_done_output().expect("cancelled output").outcome,
        ToolCallOutcome::Cancelled(_)
    ));
    assert!(pool.server_statuses()[0].connected);
    pool.shutdown_all().await;
}

#[tokio::test]
async fn dead_transport_short_circuits_before_dispatch_timeout() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "exit_after_list",
            call_timeout_ms: 1_000,
            ..MockOptions::default()
        },
    )
    .await;
    let peer = peer(&pool).await;
    wait_until(
        || peer.is_transport_closed(),
        "mock transport did not close",
    )
    .await;

    let started = Instant::now();
    let result = call(&pool).await;
    assert_eq!(failure(&result).class, ToolFailureClass::Unavailable);
    assert!(started.elapsed() < Duration::from_millis(200));
    assert!(
        pool.server_statuses()[0]
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("before tool dispatch"))
    );
    pool.shutdown_all().await;
}

#[tokio::test]
async fn interval_probe_marks_unresponsive_peer_disconnected() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            probe_interval_ms: 25,
            probe_timeout_ms: 25,
            ..MockOptions::default()
        },
    )
    .await;

    wait_until(
        || !pool.server_statuses()[0].connected,
        "background liveness probe did not disconnect peer",
    )
    .await;
    assert!(
        pool.server_statuses()[0]
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("background liveness probe failed"))
    );
    pool.shutdown_all().await;
}

#[test]
fn full_jitter_stays_within_configured_backoff() {
    for _ in 0..100 {
        assert!(full_jitter(Duration::from_millis(25)) <= Duration::from_millis(25));
    }
}
