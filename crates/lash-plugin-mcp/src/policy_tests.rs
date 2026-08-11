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

pub(super) struct ReconnectPublishHook {
    pub(super) armed: AtomicBool,
    pub(super) published: tokio::sync::Notify,
    pub(super) release: tokio::sync::Notify,
}

impl Default for ReconnectPublishHook {
    fn default() -> Self {
        Self {
            armed: AtomicBool::new(true),
            published: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        }
    }
}

const MOCK_SERVER: &str = r#"
import json, os, sys, threading, time

lock = threading.Lock()
behavior = os.environ['BEHAVIOR']
protocol = os.environ.get('PROTOCOL', '2025-11-25')
log_path = os.environ['LOG_PATH']
starts_path = os.environ['STARTS_PATH']
pid_path = os.environ.get('PID_PATH')
eof_path = os.environ.get('EOF_PATH')
if pid_path:
    with open(pid_path, 'w', encoding='utf-8') as f:
        f.write(str(os.getpid()))

try:
    with open(starts_path, 'r', encoding='utf-8') as f:
        starts = int(f.read())
except (FileNotFoundError, ValueError):
    starts = 0
with open(starts_path, 'w', encoding='utf-8') as f:
    f.write(str(starts + 1))
if behavior == 'silent_no_ping_once' and starts > 0:
    sys.exit(1)
if behavior == 'fail_once_then_success' and starts < 1:
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
        if behavior == 'crash_after_call':
            result(message['id'])
            sys.exit(0)
        else:
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
if behavior == 'ignore_eof':
    with open(eof_path, 'w', encoding='utf-8') as f:
        f.write('closed')
    time.sleep(30)
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
    reconnect_max_ms: Option<u64>,
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
            reconnect_max_ms: None,
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
            (
                "PID_PATH".to_string(),
                root.join("pid").display().to_string(),
            ),
            (
                "EOF_PATH".to_string(),
                root.join("eof").display().to_string(),
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
            reconnect_max_backoff_ms: options
                .reconnect_max_ms
                .unwrap_or(options.reconnect_initial_ms),
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
    wait_until(
        || starts(root.path()) == 2 && pool.server_statuses()[0].reconnect_exhausted,
        "reconnect cycle did not become terminal",
    )
    .await;
    let status = &pool.server_statuses()[0];
    assert!(!status.connected);
    assert!(status.last_error.is_some());
    assert!(
        status.reconnect_exhausted,
        "terminal reconnect exhaustion must be visible in public status"
    );
    let terminal = call(&pool).await;
    assert!(
        failure(&terminal)
            .message
            .contains("reconnect attempts exhausted"),
        "terminal tool-call failure must not claim recovery is active: {terminal:?}"
    );
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
async fn late_failure_after_reconnect_cannot_disconnect_healthy_service() {
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

    let stale_generation = current_entry.service_generation.load(Ordering::SeqCst);
    current_entry
        .try_connect()
        .await
        .expect("replacement connection");
    assert!(
        current_entry.service_generation.load(Ordering::SeqCst) > stale_generation,
        "replacement connection must publish a new generation"
    );
    let result = call_task.await.expect("call task");
    assert_eq!(failure(&result).class, ToolFailureClass::Timeout);
    assert!(pool.server_statuses()[0].connected);
    assert_eq!(starts(root.path()), 2);
    pool.shutdown_all().await;
}

#[tokio::test]
async fn failed_connection_attempt_reserves_a_unique_generation() {
    let root = tempfile::tempdir().unwrap();
    let entry = Arc::new(McpEntry::new(
        "mock".to_string(),
        mock_config(
            root.path(),
            MockOptions {
                behavior: "fail_once_then_success",
                ..MockOptions::default()
            },
        ),
        McpHostServices::default(),
    ));

    entry
        .establish()
        .await
        .expect_err("first attempt must fail before publication");
    assert_eq!(
        entry.generation_allocator.load(Ordering::SeqCst),
        1,
        "a failed unpublished attempt must consume its generation"
    );

    entry.establish().await.expect("replacement connection");
    assert_eq!(entry.service_generation.load(Ordering::SeqCst), 2);
    entry.cancel();
    entry.shutdown().await;
}

#[tokio::test]
async fn crash_per_call_preserves_backoff_across_successful_respawns() {
    let root = tempfile::tempdir().unwrap();
    let observed_ceilings = Arc::new(Mutex::new(Vec::new()));
    let reconnect_jitter = {
        let observed_ceilings = Arc::clone(&observed_ceilings);
        Arc::new(move |ceiling| {
            observed_ceilings.lock_recover().push(ceiling);
            Duration::ZERO
        }) as Arc<dyn Fn(Duration) -> Duration + Send + Sync>
    };
    let pool = Arc::new(McpConnectionPool::empty());
    let entry = Arc::new(
        McpEntry::new(
            "mock".to_string(),
            mock_config(
                root.path(),
                MockOptions {
                    behavior: "crash_after_call",
                    reconnect_initial_ms: 10,
                    reconnect_max_ms: Some(1_000),
                    ..MockOptions::default()
                },
            ),
            McpHostServices::default(),
        )
        .with_reconnect_jitter(reconnect_jitter),
    );
    assert!(pool.install("mock".to_string(), Arc::clone(&entry)).is_ok());
    entry.establish().await.expect("initial connection");

    for expected_starts in 2..=4 {
        let result = call(&pool).await;
        assert!(result.is_success(), "crash-after-call result: {result:?}");
        wait_until(
            || starts(root.path()) >= expected_starts && pool.server_statuses()[0].connected,
            "crash-per-call server did not reconnect",
        )
        .await;
    }

    assert_eq!(
        *observed_ceilings.lock_recover(),
        [
            Duration::from_millis(10),
            Duration::from_millis(20),
            Duration::from_millis(40),
        ],
        "short-lived successful connections must not reset reconnect pacing"
    );
    pool.shutdown_all().await;
}

#[tokio::test]
async fn disconnect_in_reconnect_publish_window_rearms_after_guard_clear() {
    let root = tempfile::tempdir().unwrap();
    let hook = Arc::new(ReconnectPublishHook::default());
    let pool = Arc::new(McpConnectionPool::empty());
    let entry = Arc::new(
        McpEntry::new(
            "mock".to_string(),
            mock_config(
                root.path(),
                MockOptions {
                    behavior: "fail_once_then_success",
                    reconnect_initial_ms: 10,
                    ..MockOptions::default()
                },
            ),
            McpHostServices::default(),
        )
        .with_reconnect_jitter(Arc::new(|_| Duration::ZERO))
        .with_reconnect_publish_hook(Arc::clone(&hook)),
    );
    assert!(pool.install("mock".to_string(), Arc::clone(&entry)).is_ok());
    entry
        .establish()
        .await
        .expect_err("the eager connection must fail");
    entry.spawn_reconnect_loop();
    tokio::time::timeout(Duration::from_secs(3), hook.published.notified())
        .await
        .expect("reconnect did not pause after publishing its service");
    assert!(pool.server_statuses()[0].connected);

    let generation = entry.service_generation.load(Ordering::SeqCst);
    assert!(
        entry
            .mark_disconnected("forced publish-window disconnect".to_string(), generation)
            .await
    );
    assert!(entry.connecting.load(Ordering::SeqCst));
    assert!(!pool.server_statuses()[0].connected);
    assert!(entry.service.lock().await.is_none());

    hook.release.notify_one();
    wait_until(
        || starts(root.path()) >= 3 && pool.server_statuses()[0].connected,
        "disconnect in reconnect publish window left the entry wedged",
    )
    .await;
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
        McpHostServices::default(),
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

#[cfg(unix)]
#[test]
fn dropping_connected_pool_and_runtime_reaps_stdio_child() {
    let root = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");
    let pool = runtime.block_on(connect_mock(
        root.path(),
        MockOptions {
            behavior: "success",
            ..MockOptions::default()
        },
    ));
    let pid: u32 = std::fs::read_to_string(root.path().join("pid"))
        .expect("stdio child must publish its pid")
        .parse()
        .expect("numeric child pid");

    drop(pool);
    drop(runtime);

    let process_path = format!("/proc/{pid}");
    let deadline = Instant::now() + Duration::from_secs(3);
    while std::path::Path::new(&process_path).exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if std::path::Path::new(&process_path).exists() {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
        panic!("stdio child PID {pid} remained alive after pool and runtime drop");
    }
}

#[cfg(unix)]
#[test]
fn runtime_drop_does_not_wait_for_in_flight_graceful_child_reap() {
    let root = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");
    let pool = runtime.block_on(connect_mock(
        root.path(),
        MockOptions {
            behavior: "ignore_eof",
            ..MockOptions::default()
        },
    ));
    let shutdown_pool = Arc::clone(&pool);
    runtime.spawn(async move {
        shutdown_pool.shutdown_all().await;
    });
    drop(pool);

    let eof_path = root.path().join("eof");
    let marker_deadline = Instant::now() + Duration::from_secs(2);
    while !eof_path.exists() && Instant::now() < marker_deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        eof_path.exists(),
        "graceful shutdown did not close child stdin"
    );

    let started = Instant::now();
    drop(runtime);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "runtime teardown waited for in-flight shutdown's graceful child reaper: {elapsed:?}"
    );
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
async fn idle_service_death_updates_status_without_a_tool_call() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "exit_after_list",
            reconnect_initial_ms: 5_000,
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

    wait_until(
        || !pool.server_statuses()[0].connected,
        "idle service death did not update pool status",
    )
    .await;
    let status = &pool.server_statuses()[0];
    assert!(
        status
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("service quit")),
        "idle death must retain its quit reason: {status:?}"
    );
    assert_eq!(
        pool.advertised_tools().len(),
        1,
        "idle death must not remove the last discovered tool catalog"
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
fn equal_jitter_stays_in_upper_half_of_configured_backoff() {
    for _ in 0..100 {
        let delay = equal_jitter(Duration::from_millis(25));
        assert!(delay >= Duration::from_millis(13));
        assert!(delay <= Duration::from_millis(25));
    }
}

#[test]
fn one_millisecond_backoff_never_jitters_to_zero() {
    for _ in 0..100 {
        assert_eq!(
            equal_jitter(Duration::from_millis(1)),
            Duration::from_millis(1)
        );
    }
}
