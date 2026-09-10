use lash_sansio::sync::{MutexExt, RwLockExt};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::{ToolCallOutcome, ToolFailure, ToolFailureClass, ToolOutcome};
use rmcp::model::{CancelledNotification, CancelledNotificationParam, ClientNotification};
use rmcp::service::{Peer, RoleClient};
use serde_json::json;
use tokio::time::Instant;

use super::*;

#[path = "policy_script.rs"]
mod scripted;

fn mcp_name(server: &str, native_tool: &str) -> String {
    crate::naming::build_prefixed_name(server, native_tool).0
}

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
    if behavior == 'success':
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
        elif behavior in ('silent_ping', 'success', 'fail_twice_then_success'):
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
        shutdown_policy: Default::default(),
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
    let name = mcp_name("mock", "work");
    pool.call_tool(
        &name,
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

/// Waits for the actor's publication cell to satisfy `condition`: a watch
/// rendezvous with the lifecycle actor, never a timed poll.
async fn service_settles(
    entry: &McpEntry,
    condition: impl FnMut(&Option<Arc<PublishedService>>) -> bool,
    message: &str,
) {
    entry
        .service
        .clone()
        .wait_for(condition)
        .await
        .expect(message);
}

async fn published_generation(entry: &McpEntry, generation: u64) {
    service_settles(
        entry,
        |service| {
            service
                .as_ref()
                .is_some_and(|service| service.generation >= generation)
        },
        "lifecycle actor publishes the awaited generation",
    )
    .await;
}

async fn unpublished(entry: &McpEntry) {
    service_settles(
        entry,
        Option::is_none,
        "lifecycle actor unpublishes the dead service",
    )
    .await;
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

#[tokio::test]
async fn progress_resets_idle_timeout_and_allows_three_times_the_idle_budget() {
    let _clock = scripted::Clock::new().await;
    let root = tempfile::tempdir().unwrap();
    let (pool, mut mock) = scripted::Mock::connect(
        root.path(),
        MockOptions {
            call_timeout_ms: 150,
            ..MockOptions::default()
        },
    )
    .await;
    let mut request = Box::pin(call(&pool));
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    mock.started(&pool).await;
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    let started = tokio::time::Instant::now();
    for _ in 0..5 {
        tokio::time::advance(Duration::from_millis(100)).await;
        mock.command("progress").await;
        // The ping response follows progress on the same stdout pipe. rmcp
        // delivers the reset before that response; polling consumes the reset.
        peer(&pool)
            .await
            .send_request(ClientRequest::PingRequest(PingRequest::default()))
            .await
            .unwrap();
        assert!(futures_util::poll!(request.as_mut()).is_pending());
    }
    mock.command("reply").await;
    let result = request.await;
    assert!(result.is_success(), "progressing call failed: {result:?}");
    assert_eq!(started.elapsed(), Duration::from_millis(500));
    drop(_clock);
    pool.shutdown_all().await;
}

#[tokio::test]
async fn progress_does_not_reset_idle_timeout_when_disabled() {
    let _clock = scripted::Clock::new().await;
    let root = tempfile::tempdir().unwrap();
    let (pool, mut mock) = scripted::Mock::connect(
        root.path(),
        MockOptions {
            reset_on_progress: false,
            ..MockOptions::default()
        },
    )
    .await;
    let mut request = Box::pin(call(&pool));
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    mock.started(&pool).await;
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    tokio::time::advance(Duration::from_millis(100)).await;
    mock.command("progress").await;
    // The ping response follows progress on the same stdout pipe. rmcp
    // delivers the reset before that response; polling consumes the reset.
    peer(&pool)
        .await
        .send_request(ClientRequest::PingRequest(PingRequest::default()))
        .await
        .unwrap();
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    tokio::time::advance(Duration::from_millis(51)).await;
    let result = request.await;
    assert_eq!(failure(&result).class, ToolFailureClass::Timeout);
    assert_eq!(failure(&result).code, "mcp_call_timeout");
    mock.event("cancelled").await;
    drop(_clock);
    pool.shutdown_all().await;
}

#[tokio::test]
async fn wall_clock_cap_fires_despite_continuous_progress() {
    let _clock = scripted::Clock::new().await;
    let root = tempfile::tempdir().unwrap();
    let (pool, mut mock) = scripted::Mock::connect(
        root.path(),
        MockOptions {
            call_max_total_timeout_ms: 350,
            policy: TimeoutDisconnectPolicy::ConsecutiveTimeouts,
            threshold: 1,
            ..MockOptions::default()
        },
    )
    .await;
    let mut request = Box::pin(call(&pool));
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    mock.started(&pool).await;
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    for _ in 0..3 {
        tokio::time::advance(Duration::from_millis(100)).await;
        mock.command("progress").await;
        // The ping response follows progress on the same stdout pipe. rmcp
        // delivers the reset before that response; polling consumes the reset.
        peer(&pool)
            .await
            .send_request(ClientRequest::PingRequest(PingRequest::default()))
            .await
            .unwrap();
        assert!(futures_util::poll!(request.as_mut()).is_pending());
    }
    tokio::time::advance(Duration::from_millis(51)).await;
    let result = request.await;
    assert_eq!(failure(&result).class, ToolFailureClass::Timeout);
    assert_eq!(failure(&result).code, "mcp_call_deadline_exceeded");
    assert!(pool.server_statuses()[0].connected);
    assert_eq!(
        entry(&pool).consecutive_timeouts.load(Ordering::SeqCst),
        0,
        "wall-cap expiry must not consume the idle-timeout budget"
    );
    mock.event("cancelled").await;
    drop(_clock);
    pool.shutdown_all().await;
}

#[tokio::test]
async fn idle_timeout_emits_cancellation_notification() {
    let clock = scripted::Clock::new().await;
    let root = tempfile::tempdir().unwrap();
    let (pool, mut mock) = scripted::Mock::connect(root.path(), MockOptions::default()).await;
    let mut request = Box::pin(call(&pool));
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    mock.started(&pool).await;
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    tokio::time::advance(Duration::from_millis(151)).await;
    let result = request.await;
    assert_eq!(failure(&result).class, ToolFailureClass::Timeout);
    mock.event("cancelled").await;
    drop(clock);
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

    let mut lifecycle = scripted::Lifecycle::new();
    lifecycle.observe(&entry(&pool));
    let result = call(&pool).await;
    assert_eq!(failure(&result).class, ToolFailureClass::Unavailable);
    lifecycle.reconnect_exhausted().await;
    assert_eq!(starts(root.path()), 2);
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
    let _clock = scripted::Clock::new().await;
    let root = tempfile::tempdir().unwrap();
    let (pool, mut mock) = scripted::Mock::connect(
        root.path(),
        MockOptions {
            call_timeout_ms: 50,
            policy: TimeoutDisconnectPolicy::ConsecutiveTimeouts,
            threshold: 2,
            reconnect_initial_ms: 10,
            ..MockOptions::default()
        },
    )
    .await;
    let reconnect_ready = Arc::new(tokio::sync::Notify::new());
    let ready = Arc::clone(&reconnect_ready);
    entry(&pool).with_reconnect_jitter(Arc::new(move |delay| {
        ready.notify_one();
        delay
    }));
    async fn expire(pool: &McpConnectionPool, mock: &mut scripted::Mock) -> ToolOutcome {
        let mut request = Box::pin(call(pool));
        assert!(futures_util::poll!(request.as_mut()).is_pending());
        mock.started(pool).await;
        assert!(futures_util::poll!(request.as_mut()).is_pending());
        tokio::time::advance(Duration::from_millis(51)).await;
        let result = request.await;
        mock.event("cancelled").await;
        result
    }
    assert_eq!(
        failure(&expire(&pool, &mut mock).await).class,
        ToolFailureClass::Timeout
    );
    *entry(&pool).last_error.write_recover() = Some("stale error".to_string());
    let mut request = Box::pin(call(&pool));
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    mock.started(&pool).await;
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    mock.command("reply").await;
    assert!(request.await.is_success());
    entry(&pool)
        .establish()
        .await
        .expect("success observation barrier");
    assert!(pool.server_statuses()[0].last_error.is_none());
    assert_eq!(
        failure(&expire(&pool, &mut mock).await).class,
        ToolFailureClass::Timeout
    );
    assert_eq!(
        failure(&expire(&pool, &mut mock).await).class,
        ToolFailureClass::Unavailable
    );
    tokio::time::resume();
    reconnect_ready.notified().await;
    let mut service = entry(&pool).service.clone();
    service
        .wait_for(Option::is_some)
        .await
        .expect("replacement publication");
    mock.reconnected().await;
    tokio::time::pause();
    assert_eq!(
        failure(&expire(&pool, &mut mock).await).class,
        ToolFailureClass::Timeout
    );
    assert!(
        pool.server_statuses()[0].connected,
        "the first timeout after reconnect must start a fresh budget"
    );
    drop(_clock);
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
    published_generation(&current_entry, 2).await;
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
    published_generation(&current_entry, 2).await;

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
    hook.reached.notified().await;
    assert!(current_entry.mark_disconnected(
        "replace generation 1 while its catalog refresh is paused".to_string(),
        1,
    ));
    published_generation(&current_entry, 2).await;
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
        [mcp_name("mock", "generation-2")]
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
    published_generation(&entry, 2).await;
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

    published_generation(&entry, 4).await;
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
        published_generation(&entry, expected_starts).await;
        assert!(starts(root.path()) >= expected_starts);
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
    published_generation(&entry, 2).await;
    assert!(pool.server_statuses()[0].connected);

    assert!(entry.mark_disconnected("forced post-publish disconnect".to_string(), 2));
    published_generation(&entry, 3).await;
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

    published_generation(&entry(&pool), 3).await;
    assert!(starts(root.path()) >= 3);
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
fn alive(pid: u32) -> bool {
    process_state(pid).is_some_and(|state| state != 'Z')
}

/// Waits for `pid` to exit, waking on the process-wide child-exit signal
/// rather than polling; returns `Some('Z')` for an unreaped child and `None`
/// once it is reaped.
#[cfg(target_os = "linux")]
async fn exited_process_state(pid: u32) -> Option<char> {
    let mut exits = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())
        .expect("observe child exits");
    loop {
        let state = process_state(pid);
        if state.is_none() || state == Some('Z') {
            return state;
        }
        exits
            .recv()
            .await
            .expect("child exit signal stream stays open");
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

    assert_eq!(
        runtime.block_on(exited_process_state(pid)),
        Some('Z'),
        "stdio child PID {pid} must be killed (zombie), not still running"
    );
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
#[tokio::test]
async fn shutdown_all_joins_actor_reaping_stdio_child() {
    let clock = scripted::Clock::new().await;
    let mut lifecycle = scripted::Lifecycle::new();
    let root = tempfile::tempdir().unwrap();
    let (pool, mut mock) = scripted::Mock::connect(
        root.path(),
        MockOptions {
            behavior: "ignore_eof",
            ..MockOptions::default()
        },
    )
    .await;
    let current_entry = entry(&pool);
    lifecycle.observe(&current_entry);

    let closed = Instant::now();
    mock.command("close").await;
    let (pid, deadline) = lifecycle.grace_armed().await;
    assert_eq!(pid, current_entry.active_pid.load(Ordering::SeqCst));
    assert_eq!(
        deadline,
        closed + Duration::from_secs(3),
        "close-ignoring child receives the configured three-second default grace"
    );
    assert!(
        current_entry.service_snapshot().is_none(),
        "actor unpublishes the service before graceful reap"
    );
    assert!(
        alive(pid),
        "actor owns a still-running child during graceful reap"
    );

    let mut shutdown = Box::pin(pool.shutdown_all());
    assert!(futures_util::poll!(shutdown.as_mut()).is_pending());
    clock.expire(deadline).await;
    assert_eq!(
        lifecycle.kill_issued(pid).await,
        Instant::now() + Duration::from_secs(1),
        "the kill request arms the one-second default cleanup deadline"
    );
    lifecycle.reaped(pid).await;
    shutdown.await;
    assert_eq!(
        Instant::now(),
        deadline + scripted::TIMER_TICK,
        "joining the reaper needs no controlled time past the grace deadline"
    );
    assert_eq!(
        process_state(pid),
        None,
        "shutdown_all joins the actor and fully reaps stdio child PID {pid}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn startup_timeout_drops_handshake_before_graceful_reap() {
    let clock = scripted::Clock::new().await;
    let mut lifecycle = scripted::Lifecycle::new();
    let root = tempfile::tempdir().unwrap();
    let pool = lifecycle.observed_pool();
    let config = mock_config(
        root.path(),
        MockOptions {
            behavior: "exit_on_eof_after_hang_initialize",
            startup_timeout_ms: 400,
            ..MockOptions::default()
        },
    );
    let mut attaching = Box::pin(pool.attach("mock".to_string(), config));
    assert!(futures_util::poll!(attaching.as_mut()).is_pending());
    let pid = lifecycle.spawned().await;
    let handshake_started = Instant::now();
    tokio::time::advance(Duration::from_millis(399)).await;
    assert!(
        futures_util::poll!(attaching.as_mut()).is_pending(),
        "startup timeout fires no earlier than the literal 400ms"
    );
    clock
        .expire(handshake_started + Duration::from_millis(400))
        .await;
    let (reaping, grace) = lifecycle.grace_armed().await;
    assert_eq!(reaping, pid);
    assert_eq!(grace, Instant::now() + Duration::from_secs(3));
    lifecycle.reaped(pid).await;
    attaching
        .await
        .expect("attach keeps a timed-out server registered");
    assert_eq!(
        Instant::now(),
        handshake_started + Duration::from_millis(400) + scripted::TIMER_TICK,
        "a child that exits on EOF is reaped without consuming the grace period"
    );

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
#[tokio::test]
async fn shutdown_during_live_handshake_reaps_actor_owned_child() {
    let _clock = scripted::Clock::new().await;
    let mut lifecycle = scripted::Lifecycle::new();
    let root = tempfile::tempdir().unwrap();
    let pool = lifecycle.observed_pool();
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
    let pid = lifecycle.spawned().await;
    let current_entry = entry(&pool);
    assert_eq!(current_entry.active_pid.load(Ordering::SeqCst), pid);
    assert!(alive(pid));

    let started = Instant::now();
    pool.shutdown_all().await;
    assert_eq!(
        started.elapsed(),
        Duration::ZERO,
        "a child that exits on EOF is reaped without consuming the grace period"
    );
    let (reaping, _) = lifecycle.grace_armed().await;
    assert_eq!(reaping, pid);
    lifecycle.reaped(pid).await;
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
#[tokio::test]
async fn non_finishing_child_reap_records_literal_pid_at_cleanup_deadline() {
    let clock = scripted::Clock::new().await;
    let mut lifecycle = scripted::Lifecycle::new();
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
    lifecycle.observe(&current_entry);
    assert!(
        pool.install("mock".to_string(), Arc::clone(&current_entry))
            .is_ok()
    );
    current_entry.establish().await.expect("connect mock");
    let pid = lifecycle.spawned().await;
    assert_eq!(pid, current_entry.active_pid.load(Ordering::SeqCst));

    let started = Instant::now();
    let mut shutdown = Box::pin(pool.shutdown_all());
    assert!(futures_util::poll!(shutdown.as_mut()).is_pending());
    let (reaping, grace) = lifecycle.grace_armed().await;
    assert_eq!(reaping, pid);
    assert_eq!(
        grace,
        started + Duration::from_secs(3),
        "the default three-second grace precedes the kill request"
    );
    clock.expire(grace).await;
    let cleanup = lifecycle.kill_issued(pid).await;
    assert_eq!(
        cleanup,
        Instant::now() + Duration::from_secs(1),
        "the default one-second cleanup deadline follows the kill request"
    );
    assert_eq!(
        exited_process_state(pid).await,
        Some('Z'),
        "the kill request lands even though the reaper refuses to observe the exit"
    );
    assert!(
        futures_util::poll!(shutdown.as_mut()).is_pending(),
        "abandonment waits for the cleanup deadline"
    );
    clock.expire(cleanup).await;
    shutdown.await;
    assert_eq!(Instant::now(), cleanup + scripted::TIMER_TICK);
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
    assert_eq!(process_state(pid), Some('Z'));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn shutdown_preempts_in_flight_keepalive_probe_and_reaps_child() {
    let clock = scripted::Clock::new().await;
    let mut lifecycle = scripted::Lifecycle::new();
    let root = tempfile::tempdir().unwrap();
    let (pool, mut mock) = scripted::Mock::connect(
        root.path(),
        MockOptions {
            behavior: "silent_ping_ignore_eof",
            probe_interval_ms: 10,
            probe_timeout_ms: 5_000,
            ..MockOptions::default()
        },
    )
    .await;
    let current_entry = entry(&pool);
    lifecycle.observe(&current_entry);
    let pid = current_entry.active_pid.load(Ordering::SeqCst);
    tokio::time::advance(Duration::from_millis(11)).await;
    mock.event("ping").await;

    let started = Instant::now();
    let mut shutdown = Box::pin(pool.shutdown_all());
    assert!(futures_util::poll!(shutdown.as_mut()).is_pending());
    let (reaping, deadline) = lifecycle.grace_armed().await;
    assert_eq!(reaping, pid);
    assert_eq!(
        deadline,
        started + Duration::from_secs(3),
        "shutdown preempts the unanswered five-second probe and starts the grace period at once"
    );
    clock.expire(deadline).await;
    lifecycle.kill_issued(pid).await;
    lifecycle.reaped(pid).await;
    shutdown.await;
    assert_eq!(Instant::now(), deadline + scripted::TIMER_TICK);
    assert_eq!(process_state(pid), None);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancelling_shutdown_owner_aborts_actor_on_live_runtime() {
    let _clock = scripted::Clock::new().await;
    let mut lifecycle = scripted::Lifecycle::new();
    let root = tempfile::tempdir().unwrap();
    let (pool, mut mock) = scripted::Mock::connect(
        root.path(),
        MockOptions {
            behavior: "ignore_eof",
            ..MockOptions::default()
        },
    )
    .await;
    let current_entry = entry(&pool);
    lifecycle.observe(&current_entry);
    let actor = current_entry
        .actor_handle
        .lock_recover()
        .as_ref()
        .expect("actor handle")
        .abort_handle();
    let shutdown_pool = Arc::clone(&pool);
    let shutdown = tokio::spawn(async move { shutdown_pool.shutdown_all().await });
    let (pid, _) = lifecycle.grace_armed().await;
    mock.event("eof").await;

    shutdown.abort();
    assert!(
        shutdown
            .await
            .expect_err("first shutdown must be cancelled")
            .is_cancelled()
    );
    lifecycle.abandoned(pid).await;
    assert!(
        actor.is_finished(),
        "cancelling the shutdown owner aborts the actor it was joining"
    );
    let started = Instant::now();
    pool.shutdown_all().await;
    assert_eq!(
        started.elapsed(),
        Duration::ZERO,
        "second shutdown returns immediately"
    );
    assert_eq!(
        exited_process_state(pid).await,
        Some('Z'),
        "abort-on-drop kills the actor-owned child rather than leaving it running"
    );
}

#[tokio::test]
async fn shutdown_all_bounds_an_actor_that_never_finishes() {
    let clock = scripted::Clock::new().await;
    let mut lifecycle = scripted::Lifecycle::new();
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
    lifecycle.observe(&entry);
    assert!(pool.install("mock".to_string(), Arc::clone(&entry)).is_ok());

    let started = Instant::now();
    let mut shutdown = Box::pin(pool.shutdown_all());
    assert!(futures_util::poll!(shutdown.as_mut()).is_pending());
    assert_eq!(lifecycle.wedged().await, 424_242);
    clock
        .elapses(shutdown.as_mut(), started, Duration::from_secs(5))
        .await;
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

#[tokio::test]
async fn shutdown_policy_shortens_shutdown_all_budget() {
    let clock = scripted::Clock::new().await;
    let mut lifecycle = scripted::Lifecycle::new();
    let root = tempfile::tempdir().unwrap();
    let pool = Arc::new(McpConnectionPool::empty());
    let shutdown_policy = McpShutdownPolicy {
        graceful_period: Duration::from_millis(50),
        post_kill_wait: Duration::from_millis(50),
    };
    let entry = McpEntry::new(
        "mock".to_string(),
        mock_config(root.path(), MockOptions::default()).with_shutdown_policy(shutdown_policy),
        McpHostServices::default(),
    )
    .with_shutdown_wedge(424_242);
    lifecycle.observe(&entry);
    assert!(pool.install("mock".to_string(), Arc::clone(&entry)).is_ok());

    let started = Instant::now();
    let mut shutdown = Box::pin(pool.shutdown_all());
    assert!(futures_util::poll!(shutdown.as_mut()).is_pending());
    assert_eq!(lifecycle.wedged().await, 424_242);
    clock
        .elapses(shutdown.as_mut(), started, Duration::from_millis(1_100))
        .await;
    assert_eq!(pool.entries.read_recover().len(), 0);
    let reason = entry
        .last_error
        .read_recover()
        .clone()
        .expect("shortened shutdown must record the abandoned actor");
    assert!(
        reason.contains("within the 1.1s per-entry total shutdown deadline"),
        "unexpected shortened shutdown reason: {reason}"
    );
}

#[tokio::test]
async fn abandoned_actor_teardown_releases_normalized_prefix_reservation() {
    let clock = scripted::Clock::new().await;
    let mut lifecycle = scripted::Lifecycle::new();
    let first_root = tempfile::tempdir().unwrap();
    let replacement_root = tempfile::tempdir().unwrap();
    let pool = Arc::new(McpConnectionPool::empty());
    let abandoned = McpEntry::new(
        "Mock Server".to_string(),
        mock_config(first_root.path(), MockOptions::default()),
        McpHostServices::default(),
    )
    .with_shutdown_wedge(333_333);
    lifecycle.observe(&abandoned);
    assert!(
        pool.install("Mock Server".to_string(), Arc::clone(&abandoned))
            .is_ok()
    );

    let started = Instant::now();
    let mut detaching = Box::pin(pool.detach("Mock Server"));
    assert!(futures_util::poll!(detaching.as_mut()).is_pending());
    assert_eq!(lifecycle.wedged().await, 333_333);
    clock
        .elapses(detaching.as_mut(), started, Duration::from_secs(5))
        .await
        .expect("detach entry");
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
#[tokio::test]
async fn eager_attach_cannot_publish_after_shutdown() {
    let clock = scripted::Clock::new().await;
    let mut lifecycle = scripted::Lifecycle::new();
    let root = tempfile::tempdir().unwrap();
    let hook = Arc::new(ActorPauseHook::default());
    let pool = lifecycle.observed_pool();
    *pool.mid_establish_hook.write_recover() = Some(Arc::clone(&hook));

    let attaching_pool = Arc::clone(&pool);
    let config = mock_config(root.path(), MockOptions::default());
    let attaching =
        tokio::spawn(async move { attaching_pool.attach("mock".to_string(), config).await });
    let pid = lifecycle.spawned().await;
    hook.reached.notified().await;
    assert_eq!(entry(&pool).active_pid.load(Ordering::SeqCst), pid);
    assert!(
        alive(pid),
        "paused lifecycle actor still owns its exact live child"
    );

    let shutdown_pool = Arc::clone(&pool);
    let shutdown = tokio::spawn(async move {
        shutdown_pool.shutdown_all().await;
    });
    let (reaping, deadline) = lifecycle.grace_armed().await;
    assert_eq!(reaping, pid);
    assert!(
        pool.entries.read_recover().is_empty(),
        "shutdown removes the attaching entry before joining its actor"
    );
    assert!(
        !attaching.is_finished(),
        "attach stays in flight until its actor reports the shutdown"
    );
    clock.expire(deadline).await;
    lifecycle.kill_issued(pid).await;
    lifecycle.reaped(pid).await;
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
#[tokio::test]
async fn aborted_mid_establish_attach_still_allows_bounded_shutdown() {
    let clock = scripted::Clock::new().await;
    let mut lifecycle = scripted::Lifecycle::new();
    let root = tempfile::tempdir().unwrap();
    let hook = Arc::new(ActorPauseHook::default());
    let pool = lifecycle.observed_pool();
    *pool.mid_establish_hook.write_recover() = Some(Arc::clone(&hook));

    let attaching_pool = Arc::clone(&pool);
    let config = mock_config(root.path(), MockOptions::default());
    let attaching =
        tokio::spawn(async move { attaching_pool.attach("mock".to_string(), config).await });
    let pid = lifecycle.spawned().await;
    hook.reached.notified().await;
    assert_eq!(entry(&pool).active_pid.load(Ordering::SeqCst), pid);
    assert!(alive(pid), "mid-establish actor owns the exact live child");

    attaching.abort();
    assert!(
        attaching
            .await
            .expect_err("aborted attach must not complete")
            .is_cancelled()
    );
    let started = Instant::now();
    let mut shutdown = Box::pin(pool.shutdown_all());
    assert!(futures_util::poll!(shutdown.as_mut()).is_pending());
    let (reaping, deadline) = lifecycle.grace_armed().await;
    assert_eq!(reaping, pid);
    assert_eq!(deadline, started + Duration::from_secs(3));
    clock.expire(deadline).await;
    lifecycle.kill_issued(pid).await;
    lifecycle.reaped(pid).await;
    shutdown.await;
    assert_eq!(
        Instant::now(),
        deadline + scripted::TIMER_TICK,
        "shutdown after an aborted attach is bounded by the grace period alone"
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
    hook.reached.notified().await;
    assert!(
        !attaching.is_finished(),
        "attach must remain in flight at the seam"
    );

    published_generation(&entry(&pool), 2).await;
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

#[tokio::test]
async fn two_wedged_entries_shutdown_concurrently_within_one_total_bound() {
    let clock = scripted::Clock::new().await;
    let mut lifecycle = scripted::Lifecycle::new();
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
    lifecycle.observe(&first);
    lifecycle.observe(&second);
    assert!(
        pool.install("first".to_string(), Arc::clone(&first))
            .is_ok()
    );
    assert!(
        pool.install("second".to_string(), Arc::clone(&second))
            .is_ok()
    );

    let started = Instant::now();
    let mut shutdown = Box::pin(pool.shutdown_all());
    assert!(futures_util::poll!(shutdown.as_mut()).is_pending());
    let mut wedged = [lifecycle.wedged().await, lifecycle.wedged().await];
    wedged.sort_unstable();
    assert_eq!(wedged, [111_111, 222_222]);
    clock
        .elapses(shutdown.as_mut(), started, Duration::from_secs(5))
        .await;
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

#[tokio::test]
async fn actor_panic_surfaces_as_join_error_and_shutdown_continues() {
    let _clock = scripted::Clock::new().await;
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
    let mut publication = entry.service.clone();
    // The publication cell closes when the panicking actor drops its sender.
    while publication.changed().await.is_ok() {}
    assert!(
        entry
            .actor_handle
            .lock_recover()
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished),
        "injected lifecycle actor panic did not occur"
    );

    let started = Instant::now();
    pool.shutdown_all().await;
    assert_eq!(
        started.elapsed(),
        Duration::ZERO,
        "a panicked actor costs shutdown no controlled time"
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

/// A host runtime built without the IO driver has no SIGCHLD stream. The
/// stdio child still connects (its pipes run on the blocking pool), and
/// `shutdown_all` must reap it through the clocked poll: no actor panic, no
/// unreaped child.
#[cfg(target_os = "linux")]
#[test]
fn shutdown_all_reaps_stdio_child_on_a_runtime_without_a_signal_driver() {
    let root = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("time-only runtime");
    let pool = runtime.block_on(connect_mock(root.path(), MockOptions::default()));
    let entry = entry(&pool);
    let pid: u32 = std::fs::read_to_string(root.path().join("pid"))
        .expect("mock records its pid")
        .trim()
        .parse()
        .expect("mock pid");
    assert!(alive(pid), "mock child runs after connect");

    runtime.block_on(pool.shutdown_all());

    assert_eq!(
        process_state(pid),
        None,
        "shutdown_all reaps the child without a SIGCHLD stream"
    );
    assert_eq!(
        entry.last_error.read_recover().as_deref(),
        None,
        "the lifecycle actor must finish shutdown without a JoinError"
    );
    assert!(
        entry.actor_handle.lock_recover().is_none(),
        "shutdown_all joins the lifecycle actor"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn runtime_drop_does_not_wait_for_in_flight_graceful_child_reap() {
    let root = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let clock = runtime.block_on(scripted::Clock::new());
    let mut lifecycle = scripted::Lifecycle::new();
    let (pool, mut mock) = runtime.block_on(scripted::Mock::connect(
        root.path(),
        MockOptions {
            behavior: "ignore_eof",
            ..MockOptions::default()
        },
    ));
    lifecycle.observe(&entry(&pool));
    let shutdown_pool = Arc::clone(&pool);
    runtime.spawn(async move {
        shutdown_pool.shutdown_all().await;
    });
    drop(pool);
    let (pid, _) = runtime.block_on(lifecycle.grace_armed());
    runtime.block_on(mock.event("eof"));
    // Release the clock's blocking task before teardown joins the blocking
    // pool; the reaper itself stays in flight inside its grace period.
    runtime.block_on(async move { drop(clock) });

    drop(runtime);
    lifecycle.abandoned_after_runtime_drop(pid);
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("observer runtime");
    assert_eq!(
        observer.block_on(exited_process_state(pid)),
        Some('Z'),
        "runtime teardown kills the abandoned child without waiting for its reaper"
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
    let _clock = scripted::Clock::new().await;
    let root = tempfile::tempdir().unwrap();
    let (pool, mut mock) = scripted::Mock::connect(
        root.path(),
        MockOptions {
            ..MockOptions::default()
        },
    )
    .await;
    let peer = peer(&pool).await;
    let mut request = Box::pin(call(&pool));
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    let event = mock.started(&pool).await;
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    peer.send_notification(ClientNotification::CancelledNotification(
        CancelledNotification::new(CancelledNotificationParam {
            request_id: serde_json::from_value(event["id"].clone()).unwrap(),
            reason: Some("test cancellation".to_string()),
        }),
    ))
    .await
    .expect("send cancellation");
    let result = request.await;
    assert!(matches!(
        result.as_done_output().expect("cancelled output").outcome,
        ToolCallOutcome::Cancelled(_)
    ));
    assert!(pool.server_statuses()[0].connected);
    drop(_clock);
    pool.shutdown_all().await;
}

#[tokio::test]
async fn dead_transport_short_circuits_before_dispatch_timeout() {
    let _clock = scripted::Clock::new().await;
    let root = tempfile::tempdir().unwrap();
    let (pool, mut mock) = scripted::Mock::connect(
        root.path(),
        MockOptions {
            call_timeout_ms: 1_000,
            ..MockOptions::default()
        },
    )
    .await;
    let mut service = entry(&pool).service.clone();
    mock.command("close").await;
    service
        .wait_for(Option::is_none)
        .await
        .expect("lifecycle acknowledges closed transport");
    let started = tokio::time::Instant::now();
    let result = call(&pool).await;
    assert_eq!(failure(&result).class, ToolFailureClass::Unavailable);
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert!(
        pool.server_statuses()[0]
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("before tool dispatch"))
    );
    drop(_clock);
    pool.shutdown_all().await;
}

#[tokio::test]
async fn idle_service_death_updates_status_without_a_tool_call() {
    let root = tempfile::tempdir().unwrap();
    let (pool, mut mock) = scripted::Mock::connect(
        root.path(),
        MockOptions {
            reconnect_initial_ms: 5_000,
            ..MockOptions::default()
        },
    )
    .await;
    mock.command("close").await;
    unpublished(&entry(&pool)).await;
    let status = &pool.server_statuses()[0];
    assert!(!status.connected);
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
    unpublished(&entry).await;

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
        [mcp_name("mock", "work")]
    );
    pool.shutdown_all().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn service_quit_records_cause_before_close_ignoring_child_cleanup() {
    let clock = scripted::Clock::new().await;
    let mut lifecycle = scripted::Lifecycle::new();
    let root = tempfile::tempdir().unwrap();
    let (pool, mut mock) = scripted::Mock::connect(
        root.path(),
        MockOptions {
            behavior: "ignore_eof",
            reconnect_initial_ms: 5_000,
            ..MockOptions::default()
        },
    )
    .await;
    let current_entry = entry(&pool);
    lifecycle.observe(&current_entry);
    let pid = current_entry.active_pid.load(Ordering::SeqCst);
    assert!(pool.server_statuses()[0].connected);

    mock.command("close").await;
    let (reaping, deadline) = lifecycle.grace_armed().await;
    assert_eq!(reaping, pid);
    let status_during_cleanup = pool.server_statuses()[0].clone();
    assert!(
        alive(pid),
        "status is sampled while the close-ignoring child is still alive"
    );

    let mut shutdown = Box::pin(pool.shutdown_all());
    assert!(futures_util::poll!(shutdown.as_mut()).is_pending());
    clock.expire(deadline).await;
    lifecycle.kill_issued(pid).await;
    lifecycle.reaped(pid).await;
    shutdown.await;

    assert!(!status_during_cleanup.connected);
    assert_eq!(
        status_during_cleanup.last_error,
        Some("MCP server `mock` service quit: Ok(Closed)".to_string()),
        "service quit cause must be visible throughout bounded child cleanup"
    );
    assert_eq!(
        process_state(pid),
        None,
        "shutdown_all must fully reap stdio child PID {pid}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn probe_loop_observes_waiting_reason_and_reaps() {
    let clock = scripted::Clock::new().await;
    let mut lifecycle = scripted::Lifecycle::new();
    let root = tempfile::tempdir().unwrap();
    let (pool, mut mock) = scripted::Mock::connect(
        root.path(),
        MockOptions {
            behavior: "silent_ping_ignore_eof",
            probe_interval_ms: 10,
            probe_timeout_ms: 5_000,
            reconnect_initial_ms: 5_000,
            ..MockOptions::default()
        },
    )
    .await;
    let current_entry = entry(&pool);
    lifecycle.observe(&current_entry);
    let reconnect_scheduled = Arc::new(tokio::sync::Notify::new());
    let observed_ceilings = Arc::new(Mutex::new(Vec::new()));
    *current_entry.reconnect_jitter.write_recover() = {
        let reconnect_scheduled = Arc::clone(&reconnect_scheduled);
        let observed_ceilings = Arc::clone(&observed_ceilings);
        Arc::new(move |ceiling| {
            observed_ceilings.lock_recover().push(ceiling);
            reconnect_scheduled.notify_one();
            ceiling
        })
    };
    let pid = current_entry.active_pid.load(Ordering::SeqCst);
    tokio::time::advance(Duration::from_millis(11)).await;
    mock.event("ping").await;

    mock.command("close").await;
    let (reaping, deadline) = lifecycle.grace_armed().await;
    assert_eq!(reaping, pid);
    assert!(
        current_entry.service_snapshot().is_none(),
        "service quit while the probe is pending must unpublish its generation"
    );
    assert_eq!(
        current_entry.last_error.read_recover().as_deref(),
        Some("MCP server `mock` service quit: Ok(Closed)"),
        "the probe loop must retain the same quit cause as the outer connected loop"
    );
    assert!(
        alive(pid),
        "the actor must retain the close-ignoring child during bounded cleanup"
    );

    clock.expire(deadline).await;
    lifecycle.kill_issued(pid).await;
    lifecycle.reaped(pid).await;
    reconnect_scheduled.notified().await;
    assert_eq!(
        *observed_ceilings.lock_recover(),
        [Duration::from_millis(5_000)],
        "a probe-loop service quit returns Disconnected and schedules the ordinary reconnect"
    );
    assert_eq!(process_state(pid), None);
    pool.shutdown_all().await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn probe_loop_observes_healthy_dwell_and_resets_reconnect_backoff() {
    let clock = scripted::Clock::new().await;
    let mut lifecycle = scripted::Lifecycle::new();
    let root = tempfile::tempdir().unwrap();
    let (pool, mut mock) = scripted::Mock::connect(
        root.path(),
        MockOptions {
            behavior: "silent_ping_ignore_eof",
            probe_interval_ms: 10,
            probe_timeout_ms: 5_000,
            reconnect_initial_ms: 10,
            reconnect_max_ms: Some(80),
            ..MockOptions::default()
        },
    )
    .await;
    let current_entry = entry(&pool);
    lifecycle.observe(&current_entry);
    let reconnect_scheduled = Arc::new(tokio::sync::Notify::new());
    let observed_ceilings = Arc::new(Mutex::new(Vec::new()));
    *current_entry.reconnect_jitter.write_recover() = {
        let reconnect_scheduled = Arc::clone(&reconnect_scheduled);
        let observed_ceilings = Arc::clone(&observed_ceilings);
        Arc::new(move |ceiling| {
            observed_ceilings.lock_recover().push(ceiling);
            reconnect_scheduled.notify_one();
            Duration::ZERO
        })
    };

    assert!(current_entry.mark_disconnected("prime reconnect backoff".to_string(), 1));
    let (first_pid, first_deadline) = lifecycle.grace_armed().await;
    clock.expire(first_deadline).await;
    lifecycle.kill_issued(first_pid).await;
    lifecycle.reaped(first_pid).await;
    reconnect_scheduled.notified().await;
    tokio::time::advance(scripted::TIMER_TICK).await;
    mock.reconnected().await;
    published_generation(&current_entry, 2).await;
    assert_eq!(
        *observed_ceilings.lock_recover(),
        [Duration::from_millis(10)],
        "the first reconnect must advance the actor's internal backoff to 20ms"
    );

    current_entry
        .establish()
        .await
        .expect("actor command barrier after generation 2 publication");
    tokio::time::advance(Duration::from_millis(11)).await;
    mock.event("ping").await;
    tokio::time::advance(Duration::from_millis(81)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        current_entry
            .service_snapshot()
            .expect("healthy dwell must not disconnect a pending probe")
            .generation,
        2
    );

    assert!(current_entry.mark_disconnected(
        "observe reconnect ceiling after healthy dwell".to_string(),
        2,
    ));
    let (second_pid, second_deadline) = lifecycle.grace_armed().await;
    clock.expire(second_deadline).await;
    lifecycle.kill_issued(second_pid).await;
    lifecycle.reaped(second_pid).await;
    reconnect_scheduled.notified().await;
    assert_eq!(
        *observed_ceilings.lock_recover(),
        [Duration::from_millis(10), Duration::from_millis(10)],
        "healthy dwell inside the pending probe must reset reconnect backoff to its initial value"
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

    unpublished(&entry(&pool)).await;
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
