use lash_sansio::sync::{MutexExt, RwLockExt};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core::{ToolCallOutcome, ToolFailure, ToolFailureClass, ToolOutcome};
use rmcp::model::{
    CancelledNotification, CancelledNotificationParam, ClientNotification, RequestId,
};
use rmcp::service::{Peer, RoleClient};
use serde_json::json;

use super::*;

pub(super) struct ActorPauseHook {
    pub(super) reached: tokio::sync::Notify,
    pub(super) release: tokio::sync::Notify,
}

impl Default for ActorPauseHook {
    fn default() -> Self {
        Self {
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        }
    }
}

impl McpEntry {
    fn with_reconnect_jitter(
        self: Arc<Self>,
        reconnect_jitter: Arc<dyn Fn(Duration) -> Duration + Send + Sync>,
    ) -> Arc<Self> {
        *self.reconnect_jitter.write_recover() = reconnect_jitter;
        self
    }

    fn with_panicking_actor(self: Arc<Self>) -> Arc<Self> {
        self.panic_actor_on_quit.store(true, Ordering::SeqCst);
        self
    }

    fn with_shutdown_wedge(self: Arc<Self>, pid: u32) -> Arc<Self> {
        self.shutdown_wedge_pid.store(pid, Ordering::SeqCst);
        self
    }

    fn with_never_finishing_child_reap(self: Arc<Self>) -> Arc<Self> {
        self.never_finish_child_reap.store(true, Ordering::SeqCst);
        self
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
close_path = os.environ.get('CLOSE_PATH')
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
if behavior == 'reset_attempts_after_success' and starts in (0, 2):
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
    if method == 'initialize' and behavior not in ('hang_initialize', 'exit_on_eof_after_hang_initialize'):
        send({'jsonrpc': '2.0', 'id': message['id'], 'result': {
            'protocolVersion': protocol,
            'capabilities': {'tools': {}},
            'serverInfo': {'name': 'policy-mock', 'version': '1.0.0'}}})
    elif method == 'tools/list':
        tool_name = 'work'
        if behavior == 'catalog_by_generation':
            tool_name = 'generation-' + str(starts + 1)
        send({'jsonrpc': '2.0', 'id': message['id'], 'result': {'tools': [{
            'name': tool_name, 'description': 'Policy test tool',
            'inputSchema': {'type': 'object', 'properties': {}}}]}})
        if behavior == 'exit_after_list' or (behavior == 'exit_after_list_once' and starts < 1):
            sys.exit(0)
        if behavior == 'reset_attempts_after_success' and starts == 1:
            sys.exit(0)
        if behavior == 'close_streams_when_triggered_after_list':
            while not os.path.exists(close_path):
                time.sleep(0.001)
            os.close(sys.stdin.fileno())
            os.close(sys.stdout.fileno())
            time.sleep(30)
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
if behavior in ('ignore_eof', 'exit_on_eof_after_hang_initialize'):
    with open(eof_path, 'w', encoding='utf-8') as f:
        f.write('closed')
if behavior == 'ignore_eof':
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
    startup_timeout_ms: u64,
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
            startup_timeout_ms: 1_000,
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
            (
                "CLOSE_PATH".to_string(),
                root.join("close").display().to_string(),
            ),
        ]),
        cwd: None,
        startup_timeout_ms: options.startup_timeout_ms,
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

async fn call(pool: &McpConnectionPool) -> ToolOutcome {
    pool.call_tool(
        "mcp__mock__work",
        &json!({}),
        &lash_core::testing::mock_attempt_context(),
    )
    .await
}

fn failure(result: &ToolOutcome) -> &ToolFailure {
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
        .service_snapshot()
        .as_ref()
        .expect("connected service")
        .peer
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
    assert_eq!(failure(&terminal).retry, ToolRetryStatus::Never);
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
    wait_until(
        || pool.server_statuses()[0].last_error.is_none(),
        "successful call was not observed by the lifecycle actor",
    )
    .await;
    assert_eq!(failure(&call(&pool).await).class, ToolFailureClass::Timeout);
    assert_eq!(
        failure(&call(&pool).await).class,
        ToolFailureClass::Unavailable
    );
    wait_until(
        || !pool.server_statuses()[0].connected,
        "timeout-threshold disconnect was not observed by the lifecycle actor",
    )
    .await;
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
            behavior: "crash_after_call",
            reconnect_initial_ms: 10,
            ..MockOptions::default()
        },
    )
    .await;
    let current_entry = entry(&pool);
    let stale_generation = current_entry
        .service_snapshot()
        .expect("initial service")
        .generation;
    assert!(call(&pool).await.is_success());
    wait_until(
        || {
            current_entry
                .service_snapshot()
                .is_some_and(|service| service.generation == 2)
        },
        "replacement connection must publish generation 2",
    )
    .await;
    assert!(!current_entry.mark_disconnected(
        "late failure from generation 1".to_string(),
        stale_generation,
    ));
    // The old publish/guard-clear race is structurally absent in the actor
    // topology. Preserve the replacement law at both fences: the caller-side
    // snapshot rejects stale work, and a stale command already in the actor
    // queue is also ignored.
    current_entry
        .actor_tx
        .send(LifecycleCommand::Disconnect {
            generation: stale_generation,
            cause: "queued late failure from generation 1".to_string(),
        })
        .expect("queue stale generation-1 disconnect");
    current_entry
        .establish()
        .await
        .expect("actor queue barrier after stale disconnect");
    assert!(pool.server_statuses()[0].connected);
    assert_eq!(
        current_entry
            .service_snapshot()
            .expect("generation 2 survives stale disconnect")
            .generation,
        2
    );
    assert_eq!(starts(root.path()), 2);
    pool.shutdown_all().await;
}

#[tokio::test]
async fn stale_generation_timeout_does_not_contaminate_replacement_accounting() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "crash_after_call",
            reconnect_initial_ms: 10,
            policy: TimeoutDisconnectPolicy::ConsecutiveTimeouts,
            threshold: 2,
            ..MockOptions::default()
        },
    )
    .await;
    let current_entry = entry(&pool);
    assert!(call(&pool).await.is_success());
    wait_until(
        || {
            current_entry
                .service_snapshot()
                .is_some_and(|service| service.generation == 2)
        },
        "replacement connection must publish generation 2",
    )
    .await;

    let (reply, result) = tokio::sync::oneshot::channel();
    current_entry
        .actor_tx
        .send(LifecycleCommand::CallTimedOut {
            generation: 1,
            reply,
        })
        .expect("queue stale generation-1 timeout");
    assert_eq!(result.await.expect("actor timeout observation reply"), None);
    assert_eq!(current_entry.consecutive_timeouts.load(Ordering::SeqCst), 0);
    pool.shutdown_all().await;
}

#[tokio::test]
async fn stale_list_changed_refresh_cannot_overwrite_replacement_catalog() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "catalog_by_generation",
            reconnect_initial_ms: 10,
            ..MockOptions::default()
        },
    )
    .await;
    let current_entry = entry(&pool);
    let initial = current_entry
        .service_snapshot()
        .expect("generation 1 service");
    assert_eq!(initial.generation, 1);
    let hook = Arc::new(ActorPauseHook::default());
    *current_entry.refresh_install_hook.write_recover() = Some(Arc::clone(&hook));

    let refreshing_entry = Arc::clone(&current_entry);
    let refresh = tokio::spawn(async move {
        refreshing_entry
            .refresh_tools(initial.peer.clone(), 1)
            .await;
    });
    tokio::time::timeout(Duration::from_secs(2), hook.reached.notified())
        .await
        .expect("generation-1 refresh did not pause before install");
    assert!(current_entry.mark_disconnected(
        "replace generation 1 while its catalog refresh is paused".to_string(),
        1,
    ));
    wait_until(
        || {
            current_entry
                .service_snapshot()
                .is_some_and(|service| service.generation == 2)
        },
        "replacement connection must publish generation 2",
    )
    .await;
    hook.release.notify_one();
    refresh.await.expect("stale refresh task");
    current_entry
        .establish()
        .await
        .expect("actor queue barrier after stale catalog install");

    assert_eq!(
        pool.advertised_tools()
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>(),
        ["mcp__mock__generation_2"]
    );
    pool.shutdown_all().await;
}

#[tokio::test]
async fn failed_connection_attempt_reserves_a_unique_generation() {
    let root = tempfile::tempdir().unwrap();
    let entry = McpEntry::new(
        "mock".to_string(),
        mock_config(
            root.path(),
            MockOptions {
                behavior: "fail_once_then_success",
                ..MockOptions::default()
            },
        ),
        McpHostServices::default(),
    )
    .with_reconnect_jitter(Arc::new(|_| Duration::ZERO));

    entry
        .establish()
        .await
        .expect_err("first attempt must fail before publication");
    wait_until(
        || {
            entry
                .service_snapshot()
                .is_some_and(|service| service.generation == 2)
        },
        "a failed unpublished attempt must consume generation 1",
    )
    .await;
    entry.shutdown().await;
}

#[tokio::test]
async fn successful_respawn_resets_reconnect_attempt_budget_but_not_generation() {
    let root = tempfile::tempdir().unwrap();
    let pool = Arc::new(McpConnectionPool::empty());
    let entry = McpEntry::new(
        "mock".to_string(),
        mock_config(
            root.path(),
            MockOptions {
                behavior: "reset_attempts_after_success",
                reconnect_initial_ms: 10,
                reconnect_max_attempts: 2,
                ..MockOptions::default()
            },
        ),
        McpHostServices::default(),
    )
    .with_reconnect_jitter(Arc::new(|_| Duration::ZERO));
    assert!(pool.install("mock".to_string(), Arc::clone(&entry)).is_ok());
    entry
        .establish()
        .await
        .expect_err("generation 1 must fail before publication");

    wait_until(
        || {
            entry
                .service_snapshot()
                .is_some_and(|service| service.generation == 4)
        },
        "successful generation 2 did not reset the reconnect-attempt budget for generations 3 and 4",
    )
    .await;
    assert_eq!(starts(root.path()), 4);
    assert!(!pool.server_statuses()[0].reconnect_exhausted);
    pool.shutdown_all().await;
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
    let entry = McpEntry::new(
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
    .with_reconnect_jitter(reconnect_jitter);
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
async fn disconnect_immediately_after_reconnect_publish_rearms_actor() {
    // The old guard-clear suppression race is structurally absent in the
    // actor topology. This replacement law proves an ordinary queued
    // post-publication disconnect rearms the same actor.
    let root = tempfile::tempdir().unwrap();
    let pool = Arc::new(McpConnectionPool::empty());
    let entry = McpEntry::new(
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
    .with_reconnect_jitter(Arc::new(|_| Duration::ZERO));
    assert!(pool.install("mock".to_string(), Arc::clone(&entry)).is_ok());
    entry
        .establish()
        .await
        .expect_err("the eager connection must fail");
    wait_until(
        || {
            entry
                .service_snapshot()
                .is_some_and(|service| service.generation == 2)
        },
        "reconnect did not publish generation 2",
    )
    .await;
    assert!(pool.server_statuses()[0].connected);

    assert!(entry.mark_disconnected("forced post-publish disconnect".to_string(), 2));
    wait_until(
        || {
            entry
                .service_snapshot()
                .is_some_and(|service| service.generation == 3)
        },
        "disconnect immediately after reconnect publication left the actor wedged",
    )
    .await;
    assert_eq!(starts(root.path()), 3);
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
async fn dropped_pool_releases_entry_and_lifecycle_actor() {
    let root = tempfile::tempdir().unwrap();
    let pool = Arc::new(McpConnectionPool::empty());
    let entry = McpEntry::new(
        "mock".to_string(),
        mock_config(
            root.path(),
            MockOptions {
                probe_interval_ms: 10,
                ..MockOptions::default()
            },
        ),
        McpHostServices::default(),
    );
    let weak = Arc::downgrade(&entry);
    assert!(pool.install("mock".to_string(), Arc::clone(&entry)).is_ok());
    drop(entry);
    drop(pool);
    assert!(weak.upgrade().is_none());
}

#[cfg(target_os = "linux")]
fn process_state(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?.1.chars().next()
}

#[cfg(target_os = "linux")]
fn wait_for_process_state(pid: u32, expected: char, timeout: Duration) -> Option<char> {
    let deadline = Instant::now() + timeout;
    loop {
        let state = process_state(pid);
        if state == Some(expected) || Instant::now() >= deadline {
            return state;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn dropping_connected_pool_kills_misbehaving_stdio_child_and_logs() {
    let traces = TraceBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(traces.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
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
    let pid: u32 = std::fs::read_to_string(root.path().join("pid"))
        .expect("stdio child must publish its pid")
        .parse()
        .expect("numeric child pid");

    drop(pool);

    let state = wait_for_process_state(pid, 'Z', Duration::from_secs(3));
    if state != Some('Z') {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
        panic!("stdio child PID {pid} must be killed (zombie), not still running; state={state:?}");
    }
    let trace = String::from_utf8(traces.0.lock_recover().clone()).unwrap();
    assert!(
        trace.contains(&format!("pid={pid}")),
        "captured trace: {trace}"
    );
    assert!(trace.contains("server=mock"), "captured trace: {trace}");
    assert!(
        trace.contains("killed without explicit pool shutdown"),
        "captured trace: {trace}"
    );

    drop(runtime);
}

#[cfg(target_os = "linux")]
#[test]
fn shutdown_all_fully_reaps_stdio_child() {
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

    runtime.block_on(pool.shutdown_all());

    assert_eq!(
        process_state(pid),
        None,
        "shutdown_all must wait for and fully reap stdio child PID {pid}"
    );
    drop(pool);
    drop(runtime);
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_all_joins_actor_reaping_stdio_child() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "close_streams_when_triggered_after_list",
            ..MockOptions::default()
        },
    )
    .await;
    let pid: u32 = std::fs::read_to_string(root.path().join("pid"))
        .expect("stdio child must publish its pid")
        .parse()
        .expect("numeric child pid");
    let entry = Arc::clone(pool.entries.read_recover().get("mock").expect("mock entry"));

    let started = Instant::now();
    std::fs::write(root.path().join("close"), "close")
        .expect("release mock to close its transport streams");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if entry.service_snapshot().is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actor must unpublish the service during graceful reap");
    assert_eq!(
        wait_for_process_state(pid, 'S', Duration::from_secs(2)),
        Some('S'),
        "actor must own a still-running child during graceful reap"
    );

    pool.shutdown_all().await;
    let elapsed = started.elapsed();
    eprintln!("actor-reap shutdown elapsed: {elapsed:?}");
    assert!(
        elapsed >= Duration::from_secs(3),
        "close-ignoring child did not receive the literal three-second grace: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "actor-reap shutdown exceeded its literal elapsed ceiling: {elapsed:?}"
    );

    assert_eq!(
        process_state(pid),
        None,
        "shutdown_all must join the actor and fully reap stdio child PID {pid}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_timeout_drops_handshake_before_graceful_reap() {
    let root = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "exit_on_eof_after_hang_initialize",
            startup_timeout_ms: 400,
            ..MockOptions::default()
        },
    )
    .await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(400),
        "startup timeout returned before the literal 400ms timeout: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(1_500),
        "startup timeout did not preserve a live grace window below the literal 1500ms ceiling: {elapsed:?}"
    );

    let pid: u32 = std::fs::read_to_string(root.path().join("pid"))
        .expect("startup-timeout child pid")
        .parse()
        .expect("numeric child pid");
    assert_eq!(
        std::fs::read_to_string(root.path().join("eof")).expect("stdin EOF marker"),
        "closed"
    );
    assert_eq!(process_state(pid), None);
    let current_entry = entry(&pool);
    assert_eq!(current_entry.active_pid.load(Ordering::SeqCst), 0);
    assert_eq!(
        current_entry.last_error.read_recover().as_deref(),
        Some("MCP startup timed out for `mock` after 400ms")
    );
    pool.shutdown_all().await;
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_during_live_handshake_reaps_actor_owned_child() {
    let root = tempfile::tempdir().unwrap();
    let pool = Arc::new(McpConnectionPool::empty());
    let attaching_pool = Arc::clone(&pool);
    let config = mock_config(
        root.path(),
        MockOptions {
            behavior: "hang_initialize",
            startup_timeout_ms: 30_000,
            ..MockOptions::default()
        },
    );
    let attaching =
        tokio::spawn(async move { attaching_pool.attach("mock".to_string(), config).await });
    wait_until(
        || {
            std::fs::read_to_string(root.path().join("pid"))
                .ok()
                .and_then(|pid| pid.parse::<u32>().ok())
                .is_some()
        },
        "handshake child did not publish its PID",
    )
    .await;
    let pid: u32 = std::fs::read_to_string(root.path().join("pid"))
        .expect("handshake child pid")
        .parse()
        .expect("numeric child pid");
    assert_eq!(
        wait_for_process_state(pid, 'S', Duration::from_secs(2)),
        Some('S')
    );
    let current_entry = entry(&pool);

    let started = Instant::now();
    pool.shutdown_all().await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "live-handshake shutdown exceeded the literal five-second bound: {elapsed:?}"
    );
    assert!(matches!(
        attaching.await.expect("attach task panicked"),
        Err(McpError::PoolShutDown)
    ));
    assert_eq!(process_state(pid), None);
    assert_eq!(
        current_entry.last_error.read_recover().as_deref(),
        Some(format!("MCP stdio child PID {pid} handshake interrupted by pool shutdown").as_str())
    );
    assert_eq!(current_entry.active_pid.load(Ordering::SeqCst), 0);
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_finishing_child_reap_records_literal_pid_at_cleanup_deadline() {
    let root = tempfile::tempdir().unwrap();
    let pool = Arc::new(McpConnectionPool::empty());
    let current_entry = McpEntry::new(
        "mock".to_string(),
        mock_config(
            root.path(),
            MockOptions {
                behavior: "ignore_eof",
                ..MockOptions::default()
            },
        ),
        McpHostServices::default(),
    )
    .with_never_finishing_child_reap();
    assert!(
        pool.install("mock".to_string(), Arc::clone(&current_entry))
            .is_ok()
    );
    current_entry.establish().await.expect("connect mock");
    let pid: u32 = std::fs::read_to_string(root.path().join("pid"))
        .expect("stdio child pid")
        .parse()
        .expect("numeric child pid");

    let started = Instant::now();
    pool.shutdown_all().await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(4),
        "non-finishing reap returned before the literal 3s + 1s cleanup deadline: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "non-finishing reap exceeded the literal five-second entry bound: {elapsed:?}"
    );
    assert_eq!(
        current_entry.last_error.read_recover().as_deref(),
        Some(
            format!(
                "MCP stdio child PID {pid} abandoned unreaped after bounded lifecycle cleanup: MCP stdio child PID {pid} did not exit within 1s after the kill request"
            )
            .as_str()
        )
    );
    assert_eq!(current_entry.active_pid.load(Ordering::SeqCst), 0);
    assert_eq!(
        wait_for_process_state(pid, 'Z', Duration::from_secs(1)),
        Some('Z')
    );
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_preempts_in_flight_keepalive_probe_and_reaps_child() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "ignore_eof",
            probe_interval_ms: 10,
            probe_timeout_ms: 5_000,
            ..MockOptions::default()
        },
    )
    .await;
    wait_until(
        || received(root.path()).contains("\"method\":\"ping\""),
        "keepalive probe did not enter its unresponsive wait",
    )
    .await;
    let pid: u32 = std::fs::read_to_string(root.path().join("pid"))
        .expect("stdio child pid")
        .parse()
        .expect("numeric child pid");

    let started = Instant::now();
    pool.shutdown_all().await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(2_900),
        "close-ignoring child did not receive its literal three-second grace: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "probe-preempting shutdown exceeded the literal five-second bound: {elapsed:?}"
    );
    assert_eq!(process_state(pid), None);
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_shutdown_owner_aborts_actor_on_live_runtime() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "ignore_eof",
            ..MockOptions::default()
        },
    )
    .await;
    let current_entry = entry(&pool);
    let actor = current_entry
        .actor_handle
        .lock_recover()
        .as_ref()
        .expect("actor handle")
        .abort_handle();
    let pid: u32 = std::fs::read_to_string(root.path().join("pid"))
        .expect("stdio child pid")
        .parse()
        .expect("numeric child pid");
    let shutdown_pool = Arc::clone(&pool);
    let shutdown = tokio::spawn(async move { shutdown_pool.shutdown_all().await });
    wait_until(
        || root.path().join("eof").exists(),
        "shutdown did not enter the child grace period",
    )
    .await;

    shutdown.abort();
    assert!(
        shutdown
            .await
            .expect_err("first shutdown must be cancelled")
            .is_cancelled()
    );
    wait_until(
        || actor.is_finished(),
        "cancelled shutdown left actor running",
    )
    .await;
    tokio::time::timeout(Duration::from_secs(1), pool.shutdown_all())
        .await
        .expect("second shutdown must return immediately");
    assert!(actor.is_finished());
    assert_eq!(
        wait_for_process_state(pid, 'Z', Duration::from_secs(2)),
        Some('Z'),
        "abort-on-drop must kill the actor-owned child rather than leave it running"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_all_bounds_an_actor_that_never_finishes() {
    let traces = TraceBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(traces.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let root = tempfile::tempdir().unwrap();
    let pool = Arc::new(McpConnectionPool::empty());
    let entry = McpEntry::new(
        "mock".to_string(),
        mock_config(root.path(), MockOptions::default()),
        McpHostServices::default(),
    )
    .with_shutdown_wedge(424_242);
    assert!(pool.install("mock".to_string(), Arc::clone(&entry)).is_ok());

    let started = Instant::now();
    pool.shutdown_all().await;
    let elapsed = started.elapsed();

    assert!(
        elapsed >= Duration::from_secs(4),
        "mock actor wait returned before the literal total bound: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(6),
        "bounded actor join exceeded its literal elapsed ceiling: {elapsed:?}"
    );
    assert_eq!(pool.entries.read_recover().len(), 0);
    assert_eq!(
        entry.last_error.read_recover().as_deref(),
        Some(
            "MCP stdio child PID 424242 abandoned: lifecycle actor did not finish within the 5s per-entry total shutdown deadline"
        )
    );
    let trace = String::from_utf8(traces.0.lock_recover().clone()).unwrap();
    assert!(
        trace.contains(
            "MCP stdio child PID 424242 abandoned: lifecycle actor did not finish within the 5s per-entry total shutdown deadline"
        ),
        "captured trace: {trace}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_actor_teardown_releases_normalized_prefix_reservation() {
    let first_root = tempfile::tempdir().unwrap();
    let replacement_root = tempfile::tempdir().unwrap();
    let pool = Arc::new(McpConnectionPool::empty());
    let abandoned = McpEntry::new(
        "Mock Server".to_string(),
        mock_config(first_root.path(), MockOptions::default()),
        McpHostServices::default(),
    )
    .with_shutdown_wedge(333_333);
    assert!(
        pool.install("Mock Server".to_string(), Arc::clone(&abandoned))
            .is_ok()
    );

    let started = Instant::now();
    pool.detach("Mock Server").await.expect("detach entry");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(4),
        "abandoned detach returned before the literal actor bound: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(6),
        "abandoned detach exceeded its literal ceiling: {elapsed:?}"
    );
    assert_eq!(pool.entries.read_recover().len(), 0);

    let replacement = McpEntry::new(
        "mock_server".to_string(),
        mock_config(replacement_root.path(), MockOptions::default()),
        McpHostServices::default(),
    );
    assert!(
        pool.install("mock_server".to_string(), Arc::clone(&replacement))
            .is_ok(),
        "abandoned entry must not retain its normalized prefix reservation"
    );
    replacement.shutdown().await;
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eager_attach_cannot_publish_after_shutdown() {
    let root = tempfile::tempdir().unwrap();
    let hook = Arc::new(ActorPauseHook::default());
    let pool = Arc::new(McpConnectionPool::empty());
    *pool.mid_establish_hook.write_recover() = Some(Arc::clone(&hook));

    let attaching_pool = Arc::clone(&pool);
    let config = mock_config(root.path(), MockOptions::default());
    let attaching =
        tokio::spawn(async move { attaching_pool.attach("mock".to_string(), config).await });
    tokio::time::timeout(Duration::from_secs(2), hook.reached.notified())
        .await
        .expect("eager attach actor did not pause mid-establish");
    let pid: u32 = std::fs::read_to_string(root.path().join("pid"))
        .expect("stdio child must publish its pid")
        .parse()
        .expect("numeric child pid");
    assert_eq!(
        wait_for_process_state(pid, 'S', Duration::from_secs(2)),
        Some('S'),
        "paused lifecycle actor must still own its exact live child"
    );

    let shutdown_pool = Arc::clone(&pool);
    let shutdown = tokio::spawn(async move {
        shutdown_pool.shutdown_all().await;
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if pool.entries.read_recover().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown did not remove the attaching entry");
    let attach_result = attaching.await.expect("attach task panicked");
    shutdown.await.expect("shutdown task panicked");

    assert!(matches!(attach_result, Err(McpError::PoolShutDown)));
    assert_eq!(pool.entries.read_recover().len(), 0);
    assert_eq!(
        process_state(pid),
        None,
        "shutdown must fully reap the eager attach child PID {pid}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborted_mid_establish_attach_still_allows_bounded_shutdown() {
    let root = tempfile::tempdir().unwrap();
    let hook = Arc::new(ActorPauseHook::default());
    let pool = Arc::new(McpConnectionPool::empty());
    *pool.mid_establish_hook.write_recover() = Some(Arc::clone(&hook));

    let attaching_pool = Arc::clone(&pool);
    let config = mock_config(root.path(), MockOptions::default());
    let attaching =
        tokio::spawn(async move { attaching_pool.attach("mock".to_string(), config).await });
    tokio::time::timeout(Duration::from_secs(2), hook.reached.notified())
        .await
        .expect("lifecycle actor did not pause mid-establish");
    let pid: u32 = std::fs::read_to_string(root.path().join("pid"))
        .expect("stdio child must publish its pid")
        .parse()
        .expect("numeric child pid");
    assert_eq!(
        wait_for_process_state(pid, 'S', Duration::from_secs(2)),
        Some('S'),
        "mid-establish actor must own the exact live child"
    );

    attaching.abort();
    assert!(
        attaching
            .await
            .expect_err("aborted attach must not complete")
            .is_cancelled()
    );
    let started = Instant::now();
    pool.shutdown_all().await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(6),
        "shutdown after an aborted attach exceeded its literal ceiling: {elapsed:?}"
    );
    assert_eq!(pool.entries.read_recover().len(), 0);
    assert_eq!(
        process_state(pid),
        None,
        "shutdown must fully reap the actor-owned mid-establish child PID {pid}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish_then_die_while_attach_is_in_flight_reconnects() {
    let root = tempfile::tempdir().unwrap();
    let hook = Arc::new(ActorPauseHook::default());
    let pool = Arc::new(McpConnectionPool::empty());
    *pool.attach_return_hook.write_recover() = Some(Arc::clone(&hook));

    let attaching_pool = Arc::clone(&pool);
    let config = mock_config(
        root.path(),
        MockOptions {
            behavior: "exit_after_list_once",
            reconnect_initial_ms: 10,
            ..MockOptions::default()
        },
    );
    let attaching =
        tokio::spawn(async move { attaching_pool.attach("mock".to_string(), config).await });
    tokio::time::timeout(Duration::from_secs(2), hook.reached.notified())
        .await
        .expect("attach did not pause after the first service publication");
    assert!(
        !attaching.is_finished(),
        "attach must remain in flight at the seam"
    );

    wait_until(
        || {
            entry(&pool)
                .service_snapshot()
                .is_some_and(|service| service.generation == 2)
        },
        "published service death did not trigger generation 2 while attach was in flight",
    )
    .await;
    assert_eq!(starts(root.path()), 2);
    assert!(
        !attaching.is_finished(),
        "reconnect must not depend on attach returning"
    );

    hook.release.notify_one();
    assert!(matches!(
        attaching.await.expect("attach task panicked"),
        Ok(())
    ));
    pool.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_wedged_entries_shutdown_concurrently_within_one_total_bound() {
    let first_root = tempfile::tempdir().unwrap();
    let second_root = tempfile::tempdir().unwrap();
    let pool = Arc::new(McpConnectionPool::empty());
    let first = McpEntry::new(
        "first".to_string(),
        mock_config(first_root.path(), MockOptions::default()),
        McpHostServices::default(),
    )
    .with_shutdown_wedge(111_111);
    let second = McpEntry::new(
        "second".to_string(),
        mock_config(second_root.path(), MockOptions::default()),
        McpHostServices::default(),
    )
    .with_shutdown_wedge(222_222);
    assert!(
        pool.install("first".to_string(), Arc::clone(&first))
            .is_ok()
    );
    assert!(
        pool.install("second".to_string(), Arc::clone(&second))
            .is_ok()
    );

    let started = Instant::now();
    pool.shutdown_all().await;
    let elapsed = started.elapsed();

    assert!(
        elapsed >= Duration::from_secs(4),
        "wedged actors returned before the literal total bound: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(6),
        "two wedged actors serialized their total bounds: {elapsed:?}"
    );
    assert_eq!(pool.entries.read_recover().len(), 0);
    assert_eq!(
        first.last_error.read_recover().as_deref(),
        Some(
            "MCP stdio child PID 111111 abandoned: lifecycle actor did not finish within the 5s per-entry total shutdown deadline"
        )
    );
    assert_eq!(
        second.last_error.read_recover().as_deref(),
        Some(
            "MCP stdio child PID 222222 abandoned: lifecycle actor did not finish within the 5s per-entry total shutdown deadline"
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actor_panic_surfaces_as_join_error_and_shutdown_continues() {
    let root = tempfile::tempdir().unwrap();
    let pool = Arc::new(McpConnectionPool::empty());
    let entry = McpEntry::new(
        "mock".to_string(),
        mock_config(
            root.path(),
            MockOptions {
                behavior: "exit_after_list",
                ..MockOptions::default()
            },
        ),
        McpHostServices::default(),
    )
    .with_panicking_actor();
    assert!(pool.install("mock".to_string(), Arc::clone(&entry)).is_ok());
    entry.establish().await.expect("initial connection");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if entry
                .actor_handle
                .lock_recover()
                .as_ref()
                .is_some_and(tokio::task::JoinHandle::is_finished)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("injected lifecycle actor panic did not occur");
    let started = Instant::now();
    tokio::time::timeout(Duration::from_secs(2), pool.shutdown_all())
        .await
        .expect("panicked actor made shutdown exceed its literal ceiling");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "actor panic shutdown exceeded its literal elapsed ceiling: {elapsed:?}"
    );
    assert!(
        entry
            .last_error
            .read_recover()
            .as_deref()
            .is_some_and(|error| error.contains("JoinError")),
        "actor panic must surface through its retained JoinHandle"
    );
    assert_eq!(pool.entries.read_recover().len(), 0);
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
    eprintln!("runtime teardown elapsed: {elapsed:?}");
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
            behavior: "close_streams_when_triggered_after_list",
            call_timeout_ms: 1_000,
            ..MockOptions::default()
        },
    )
    .await;
    let peer = peer(&pool).await;
    std::fs::write(root.path().join("close"), "close")
        .expect("release mock to close its transport streams");
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
            behavior: "close_streams_when_triggered_after_list",
            reconnect_initial_ms: 5_000,
            ..MockOptions::default()
        },
    )
    .await;
    let peer = peer(&pool).await;
    std::fs::write(root.path().join("close"), "close")
        .expect("release mock to close its transport streams");
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
async fn discovery_publishes_received_catalog_before_observing_same_burst_quit() {
    let root = tempfile::tempdir().unwrap();
    let pool = Arc::new(McpConnectionPool::empty());
    let entry = McpEntry::new(
        "mock".to_string(),
        mock_config(
            root.path(),
            MockOptions {
                behavior: "exit_after_list",
                reconnect_initial_ms: 5_000,
                ..MockOptions::default()
            },
        ),
        McpHostServices::default(),
    );
    assert!(pool.install("mock".to_string(), Arc::clone(&entry)).is_ok());

    entry
        .establish()
        .await
        .expect("a received tools/list catalog must publish before transport EOF is observed");
    wait_until(
        || !pool.server_statuses()[0].connected,
        "same-burst service quit was not observed after catalog publication",
    )
    .await;

    let status = &pool.server_statuses()[0];
    assert!(!status.connected);
    assert_eq!(
        status.last_error,
        Some("MCP server `mock` service quit: Ok(Closed)".to_string())
    );
    assert_eq!(status.tool_count, 1);
    assert_eq!(
        pool.advertised_tools()
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>(),
        ["mcp__mock__work"]
    );
    pool.shutdown_all().await;
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_quit_records_cause_before_close_ignoring_child_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let pool = connect_mock(
        root.path(),
        MockOptions {
            behavior: "close_streams_when_triggered_after_list",
            reconnect_initial_ms: 5_000,
            ..MockOptions::default()
        },
    )
    .await;
    let pid: u32 = std::fs::read_to_string(root.path().join("pid"))
        .expect("stdio child must publish its pid")
        .parse()
        .expect("numeric child pid");
    assert!(pool.server_statuses()[0].connected);

    std::fs::write(root.path().join("close"), "close")
        .expect("release mock to close its transport streams");
    wait_until(
        || !pool.server_statuses()[0].connected,
        "service quit did not unpublish the connection",
    )
    .await;
    let status_during_cleanup = pool.server_statuses()[0].clone();
    let process_state_during_cleanup = wait_for_process_state(pid, 'S', Duration::from_secs(2));

    pool.shutdown_all().await;

    assert!(!status_during_cleanup.connected);
    assert_eq!(
        status_during_cleanup.last_error,
        Some("MCP server `mock` service quit: Ok(Closed)".to_string()),
        "service quit cause must be visible throughout bounded child cleanup"
    );
    assert_eq!(
        process_state_during_cleanup,
        Some('S'),
        "status must be sampled while the close-ignoring child is still alive"
    );
    assert_eq!(
        process_state(pid),
        None,
        "shutdown_all must fully reap stdio child PID {pid}"
    );
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
