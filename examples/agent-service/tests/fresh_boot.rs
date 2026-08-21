use std::fs::File;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
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

    let child = Command::new(env!("CARGO_BIN_EXE_agent-service"))
        .env("OPENROUTER_API_KEY", "test-key")
        .env("AGENT_SERVICE_DURABILITY", "local")
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
                std::fs::read_to_string(&stdout_path).expect("read agent-service stdout"),
                std::fs::read_to_string(&stderr_path).expect("read agent-service stderr"),
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn unused_local_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve local address");
    listener.local_addr().expect("read local address")
}
