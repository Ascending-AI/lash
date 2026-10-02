use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn fresh_data_dir_boot_reaches_listener() {
    let data_dir = tempfile::tempdir().expect("fresh agent-service data dir");
    let output_dir = tempfile::tempdir().expect("agent-service output dir");
    let stdout_path = output_dir.path().join("stdout.log");
    let stderr_path = output_dir.path().join("stderr.log");
    let addr = unused_local_addr();
    let endpoint_addr = unused_local_addr();
    // No restate-server runs here; a stub answers the boot's only server
    // call, registering the deployment: the empty listing, a 404 on every
    // service lookup, and an accepted `POST /deployments`. Turn execution
    // never reaches the stub — this boot sends nothing.
    let restate_admin = RestateAdminStub::start();
    let unreachable_restate = format!("http://{}", unused_local_addr());

    let child = Command::new(env!("CARGO_BIN_EXE_agent-service"))
        .env("OPENROUTER_API_KEY", "test-key")
        .env("RESTATE_AUTHORITY_ID", "agent-service-fresh-boot")
        .env("RESTATE_INGRESS_URL", &unreachable_restate)
        .env("RESTATE_ADMIN_URL", restate_admin.url())
        .env("AGENT_SERVICE_RESTATE_ADDR", endpoint_addr.to_string())
        .env("AGENT_SERVICE_DATA_DIR", data_dir.path())
        .env("AGENT_SERVICE_ADDR", addr.to_string())
        .stdout(Stdio::from(
            File::create(&stdout_path).expect("agent-service stdout log"),
        ))
        .stderr(Stdio::from(
            File::create(&stderr_path).expect("agent-service stderr log"),
        ))
        .spawn()
        .expect("launch agent-service");
    let mut child = ChildGuard(child);
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        if TcpStream::connect(addr).is_ok() {
            assert!(
                data_dir
                    .path()
                    .join("lash-sessions/durable-core.db")
                    .is_file(),
                "real boot must create the shared session catalog"
            );
            assert_eq!(
                restate_admin.registered_uris(),
                vec![format!("http://{endpoint_addr}")],
                "the boot registers its bound endpoint with the server"
            );
            return;
        }
        if let Some(status) = child.0.try_wait().expect("poll agent-service") {
            panic!(
                "agent-service exited before listening ({status})\nstdout:\n{}\nstderr:\n{}",
                std::fs::read_to_string(&stdout_path).expect("read agent-service stdout"),
                std::fs::read_to_string(&stderr_path).expect("read agent-service stderr"),
            );
        }
        if Instant::now() >= deadline {
            panic!(
                "agent-service did not listen at {addr}\nstdout:\n{}\nstderr:\n{}",
                std::fs::read_to_string(&stdout_path).expect("agent-service stdout"),
                std::fs::read_to_string(&stderr_path).expect("agent-service stderr"),
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// The slice of the Restate admin API a boot performs when it serves its
/// endpoint: `GET /deployments` (empty), `GET /services/{name}` (absent), and
/// `POST /deployments {uri}` (accepted, its URI recorded). One request per
/// connection; `connection: close` keeps the client off keep-alive.
struct RestateAdminStub {
    addr: SocketAddr,
    registered: Arc<Mutex<Vec<String>>>,
}

#[expect(
    clippy::expect_used,
    reason = "test setup: a loopback bind, request head or socket clone that fails means the test environment is broken"
)]
impl RestateAdminStub {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind Restate admin stub");
        let addr = listener.local_addr().expect("stub local address");
        let registered = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&registered);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                Self::answer(&mut stream, &seen);
            }
        });
        Self { addr, registered }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn registered_uris(&self) -> Vec<String> {
        self.registered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn answer(stream: &mut TcpStream, registered: &Mutex<Vec<String>>) {
        let Some((method, path, body)) = read_request(stream) else {
            return;
        };
        let (status, payload) = match (method.as_str(), path.as_str()) {
            ("GET", "/deployments") => ("200 OK", "{\"deployments\":[]}".to_string()),
            ("POST", "/deployments") => {
                let uri = serde_json::from_slice::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|body| body.get("uri")?.as_str().map(str::to_string));
                if let Some(uri) = uri {
                    registered
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(uri);
                }
                ("201 Created", "{}".to_string())
            }
            _ => ("404 Not Found", "{\"message\":\"not found\"}".to_string()),
        };
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                payload.len()
            )
            .as_bytes(),
        );
        let _ = stream.shutdown(Shutdown::Both);
    }
}

/// One HTTP/1.1 request: head to `\r\n\r\n`, then `content-length` body
/// bytes. Returns the method, path and body.
#[expect(
    clippy::expect_used,
    reason = "test stub: a truncated request from the child under test is a broken test environment"
)]
fn read_request(stream: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .expect("clone the request stream for reading"),
    );
    let mut head = String::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).expect("read request line") == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if head.is_empty() {
            head = line.to_string();
        } else if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).expect("read request body");
    let mut parts = head.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    Some((method, path, body))
}

/// Test-support helper outside `#[test]`, so clippy.toml's allow-in-tests does
/// not reach it: binding port 0 on loopback cannot fail but for OS exhaustion.
#[expect(
    clippy::expect_used,
    reason = "loopback bind on port 0 and the OS-assigned local_addr it returns are infallible by construction"
)]
fn unused_local_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve local address");
    listener.local_addr().expect("read local address")
}
