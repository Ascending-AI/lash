//! A subprocess controlled by messages while Tokio's deadline clock is frozen.
use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

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
impl Drop for Clock {
    fn drop(&mut self) {
        tokio::time::resume();
        let _ = self.0.send(());
    }
}

pub(super) struct Mock {
    listener: TcpListener,
    stream: BufReader<TcpStream>,
}
impl Mock {
    pub(super) async fn connect(
        root: &Path,
        options: MockOptions,
    ) -> (Arc<McpConnectionPool>, Self) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = mock_config(root, options);
        let McpServerConfig::Stdio { args, env, .. } = &mut config else {
            unreachable!()
        };
        *args = vec!["-u".into(), "-c".into(), SERVER.into()];
        env.insert(
            "CONTROL_PORT".into(),
            listener.local_addr().unwrap().port().to_string(),
        );
        let pool = McpConnectionPool::connect(BTreeMap::from([("mock".into(), config)]))
            .await
            .unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        assert!(pool.server_statuses()[0].connected);
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
import json, os, socket, sys, threading
control = socket.create_connection(('127.0.0.1', int(os.environ['CONTROL_PORT'])))
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
            'protocolVersion':'2025-11-25','capabilities':{'tools':{}},
            'serverInfo':{'name':'scripted-policy-mock','version':'1'}}})
    elif method == 'tools/list':
        send({'jsonrpc':'2.0','id':message['id'],'result':{'tools':[{
            'name':'work','inputSchema':{'type':'object'}}]}})
    elif method == 'tools/call':
        current = message
        event('call', id=message['id'])
    elif method == 'ping':
        send({'jsonrpc':'2.0','id':message['id'],'result':{}})
    elif method == 'notifications/cancelled':
        event('cancelled')
event('eof')
if os.environ['BEHAVIOR'] == 'ignore_eof':
    threading.Event().wait()
"#;
