//! A subprocess controlled by messages while Tokio's deadline clock is frozen.
use super::*;
use crate::service_lifecycle::{LifecycleEvent, LifecycleObserver};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;

/// Tokio rounds every timer deadline up to the next millisecond tick, so the
/// earliest controlled instant at which `sleep_until(deadline)` has certainly
/// fired is one millisecond past the deadline.
pub(super) const TIMER_TICK: Duration = Duration::from_millis(1);

// An active blocking task inhibits Tokio's automatic time advance. It waits on
// a channel, so real subprocess I/O can finish without consuming policy time.
pub(super) struct Clock(std::sync::mpsc::Sender<()>);
impl Clock {
    pub(super) async fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let (ready, started) = tokio::sync::oneshot::channel();
        tokio::task::spawn_blocking(move || {
            ready.send(()).unwrap();
            let _ = rx.recv();
        });
        started.await.unwrap();
        tokio::time::pause();
        Self(tx)
    }
}
impl Clock {
    /// Crosses `deadline`: afterwards every timer armed for it has fired.
    pub(super) async fn expire(&self, deadline: Instant) {
        tokio::time::advance(deadline + TIMER_TICK - Instant::now()).await;
    }

    /// Drives an already-polled `future` across a bound armed at `started`:
    /// it must still be pending one tick before the bound and must complete
    /// once the bound is crossed, without any further controlled time.
    pub(super) async fn elapses<F: std::future::Future>(
        &self,
        mut future: std::pin::Pin<&mut F>,
        started: Instant,
        bound: Duration,
    ) -> F::Output {
        tokio::time::advance(started + bound - TIMER_TICK - Instant::now()).await;
        assert!(
            futures_util::poll!(future.as_mut()).is_pending(),
            "completed before its {bound:?} bound"
        );
        self.expire(started + bound).await;
        let output = future.await;
        assert_eq!(Instant::now(), started + bound + TIMER_TICK);
        output
    }
}

impl Drop for Clock {
    fn drop(&mut self) {
        tokio::time::resume();
        let _ = self.0.send(());
    }
}

/// Receives lifecycle transitions from the entries a test owns, so the test
/// learns each reap deadline from the actor that armed it and waits for child
/// exits as events instead of polling process state.
pub(super) struct Lifecycle {
    observer: LifecycleObserver,
    events: UnboundedReceiver<LifecycleEvent>,
}

impl Lifecycle {
    pub(super) fn new() -> Self {
        let (observer, events) = tokio::sync::mpsc::unbounded_channel();
        Self { observer, events }
    }

    /// An empty pool whose future entries report to this observer.
    pub(super) fn observed_pool(&self) -> Arc<McpConnectionPool> {
        let pool = McpConnectionPool::empty();
        *pool.lifecycle_observer.write_recover() = Some(self.observer.clone());
        Arc::new(pool)
    }

    pub(super) fn observe(&self, entry: &McpEntry) {
        *entry.lifecycle_observer.write_recover() = Some(self.observer.clone());
    }

    async fn next(&mut self) -> LifecycleEvent {
        self.events
            .recv()
            .await
            .expect("lifecycle observer outlives every observed entry")
    }

    pub(super) async fn spawned(&mut self) -> u32 {
        loop {
            if let LifecycleEvent::Spawned { pid } = self.next().await {
                return pid;
            }
        }
    }

    pub(super) async fn grace_armed(&mut self) -> (u32, Instant) {
        loop {
            if let LifecycleEvent::GraceArmed { pid, deadline } = self.next().await {
                return (pid, deadline);
            }
        }
    }

    pub(super) async fn term_issued(&mut self, pid: u32) -> Instant {
        loop {
            match self.next().await {
                LifecycleEvent::TermIssued {
                    pid: termed,
                    deadline,
                } => {
                    assert_eq!(termed, pid);
                    return deadline;
                }
                LifecycleEvent::Reaped { pid: reaped } => {
                    panic!("child {reaped} was reaped before the terminate request")
                }
                _ => {}
            }
        }
    }

    pub(super) async fn kill_issued(&mut self, pid: u32) -> Instant {
        loop {
            match self.next().await {
                LifecycleEvent::KillIssued {
                    pid: killed,
                    deadline,
                } => {
                    assert_eq!(killed, pid);
                    return deadline;
                }
                LifecycleEvent::Reaped { pid: reaped } => {
                    panic!("child {reaped} was reaped before the kill request")
                }
                _ => {}
            }
        }
    }

    pub(super) async fn reaped(&mut self, pid: u32) {
        loop {
            match self.next().await {
                LifecycleEvent::Reaped { pid: reaped } => {
                    assert_eq!(reaped, pid);
                    return;
                }
                LifecycleEvent::Abandoned { pid: abandoned } => {
                    panic!("child {abandoned} was abandoned while awaiting the reap of {pid}")
                }
                _ => {}
            }
        }
    }

    pub(super) async fn abandoned(&mut self, pid: u32) {
        loop {
            match self.next().await {
                LifecycleEvent::Abandoned { pid: abandoned } => {
                    assert_eq!(abandoned, pid);
                    return;
                }
                LifecycleEvent::Reaped { pid: reaped } => {
                    panic!("child {reaped} was reaped while awaiting the abandonment of {pid}")
                }
                _ => {}
            }
        }
    }

    /// Same as [`Lifecycle::abandoned`], for events emitted while a runtime
    /// was torn down and no runtime is left to await on.
    pub(super) fn abandoned_after_runtime_drop(&mut self, pid: u32) {
        loop {
            match self.events.try_recv() {
                Ok(LifecycleEvent::Abandoned { pid: abandoned }) => {
                    assert_eq!(abandoned, pid);
                    return;
                }
                Ok(LifecycleEvent::Reaped { pid: reaped }) => {
                    panic!("child {reaped} was reaped while awaiting the abandonment of {pid}")
                }
                Ok(_) => {}
                Err(error) => panic!("runtime teardown did not abandon child {pid}: {error}"),
            }
        }
    }

    pub(super) async fn wedged(&mut self) -> u32 {
        loop {
            if let LifecycleEvent::Wedged { pid } = self.next().await {
                return pid;
            }
        }
    }

    pub(super) async fn reconnect_exhausted(&mut self) {
        loop {
            if let LifecycleEvent::ReconnectExhausted = self.next().await {
                return;
            }
        }
    }

    /// The deadline of the next reconnect attempt the actor armed. Crossing it
    /// with [`Clock::expire`] starts that attempt; its handshake then runs in
    /// real time while the startup timeout stays frozen.
    pub(super) async fn reconnect_scheduled(&mut self) -> Instant {
        loop {
            if let LifecycleEvent::ReconnectScheduled { deadline } = self.next().await {
                return deadline;
            }
        }
    }
}

/// The control channel is a Unix socket inside the test's own temporary
/// directory. A loopback TCP port is reachable by any local process, so a
/// port prober (an editor's port forwarder, an HTTP health check) could win
/// the accept and feed the test its bytes instead of the mock's events.
pub(super) struct Mock {
    listener: UnixListener,
    stream: BufReader<UnixStream>,
}
impl Mock {
    pub(super) async fn connect(
        root: &Path,
        options: MockOptions,
    ) -> (Arc<McpConnectionPool>, Self) {
        let control = root.join("control.sock");
        let listener = UnixListener::bind(&control).unwrap();
        let mut config = mock_config(root, options);
        let McpTransport::Stdio(transport) = &mut config.transport else {
            unreachable!()
        };
        transport.args = vec!["-u".into(), "-c".into(), SERVER.into()];
        let env = &mut transport.env;
        env.insert(
            "CONTROL_SOCKET".into(),
            control.to_str().unwrap().to_string(),
        );
        let pool = McpConnectionPool::connect(BTreeMap::from([("mock".into(), config)]))
            .await
            .unwrap();
        assert_connected(&pool);
        let (stream, _) = listener.accept().await.unwrap();
        (
            pool,
            Self {
                listener,
                stream: BufReader::new(stream),
            },
        )
    }

    pub(super) async fn started(&mut self, pool: &McpConnectionPool) -> serde_json::Value {
        let event = self.event("call").await;
        peer(pool)
            .await
            .send_request(ClientRequest::PingRequest(PingRequest::default()))
            .await
            .unwrap();
        event
    }

    pub(super) async fn event(&mut self, expected: &str) -> serde_json::Value {
        let mut line = String::new();
        assert_ne!(
            self.stream.read_line(&mut line).await.unwrap(),
            0,
            "mock control closed before {expected}"
        );
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(event["event"], expected, "{event}");
        event
    }

    pub(super) async fn command(&mut self, command: &str) {
        self.stream
            .get_mut()
            .write_all(format!("{command}\n").as_bytes())
            .await
            .unwrap();
        self.event(command).await;
    }

    pub(super) async fn reconnected(&mut self) {
        let (stream, _) = self.listener.accept().await.unwrap();
        self.stream = BufReader::new(stream);
    }
}

const SERVER: &str = r#"
import json, os, signal, socket, sys, threading
# The mock ignores SIGTERM so scripted tests deterministically reach the
# SIGKILL stage of forced shutdown; graceful stdin-EOF exit is unchanged.
signal.signal(signal.SIGTERM, signal.SIG_IGN)
behavior = os.environ['BEHAVIOR']
starts_path = os.environ['STARTS_PATH']
try:
    with open(starts_path, 'r', encoding='utf-8') as f:
        starts = int(f.read())
except (FileNotFoundError, ValueError):
    starts = 0
with open(starts_path, 'w', encoding='utf-8') as f:
    f.write(str(starts + 1))
if behavior == 'unanswered_ping_once' and starts > 0:
    sys.exit(1)
# Behaviors that report every liveness ping on the control channel, so a test
# learns the probe (and its frozen timeout) is armed before it moves time.
probed = ('answered_ping', 'ping_error', 'ping_meta', 'unanswered_ping',
          'unanswered_ping_once', 'silent_ping_ignore_eof')
unanswered = ('unanswered_ping', 'unanswered_ping_once', 'silent_ping_ignore_eof')
control = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
control.connect(os.environ['CONTROL_SOCKET'])
output_lock = threading.Lock()
event_lock = threading.Lock()
current = None

def send(message):
    with output_lock:
        sys.stdout.write(json.dumps(message) + '\n')
        sys.stdout.flush()

def event(name, **fields):
    with event_lock:
        control.sendall((json.dumps(dict(event=name, **fields)) + '\n').encode())

def commands():
    for line in control.makefile('r'):
        command = line.strip()
        if command == 'reply':
            send({'jsonrpc':'2.0','id':current['id'],'result':{'content':[{'type':'text','text':'ok'}]}})
        elif command == 'progress':
            send({'jsonrpc':'2.0','method':'notifications/progress','params':{
                'progressToken':current['params']['_meta']['progressToken'],'progress':1}})
        elif command == 'close':
            # Closing stdout can make the host close stdin immediately. Publish
            # this transition before the stdin reader acknowledges EOF.
            with event_lock:
                os.close(0)
                os.close(1)
                control.sendall((json.dumps({'event':'close'}) + '\n').encode())
            continue
        event(command)
threading.Thread(target=commands, daemon=True).start()
for line in sys.stdin:
    message = json.loads(line)
    method = message.get('method')
    if method == 'initialize':
        send({'jsonrpc':'2.0','id':message['id'],'result':{
            'protocolVersion':os.environ['PROTOCOL'],'capabilities':{'tools':{}},
            'serverInfo':{'name':'scripted-policy-mock','version':'1'}}})
    elif method == 'tools/list':
        send({'jsonrpc':'2.0','id':message['id'],'result':{'tools':[{
            'name':'work','inputSchema':{'type':'object'}}]}})
    elif method == 'tools/call':
        current = message
        event('call', id=message['id'])
    elif method == 'ping':
        if behavior in probed:
            event('ping')
        if behavior in unanswered:
            continue
        if behavior == 'ping_error':
            send({'jsonrpc':'2.0','id':message['id'],
                  'error':{'code':-32601,'message':'Method not found'}})
        elif behavior == 'ping_meta':
            send({'jsonrpc':'2.0','id':message['id'],'result':{'_meta':{'alive':True}}})
        else:
            send({'jsonrpc':'2.0','id':message['id'],'result':{}})
    elif method == 'notifications/cancelled':
        event('cancelled')
event('eof')
if behavior in ('ignore_eof', 'silent_ping_ignore_eof'):
    threading.Event().wait()
"#;
