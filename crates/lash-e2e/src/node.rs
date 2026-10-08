//! One booted host process: one lash node.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use anyhow::{Context as _, Result, bail, ensure};
use serde_json::Value;

/// Which host binary a node runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Host {
    /// `examples/e2e-consumer`: a host built from the public facade alone.
    Consumer,
    /// `examples/agent-workbench`, with its `e2e-tools` fixtures.
    Workbench,
}

impl Host {
    /// The binary's path and digest variables the E2E runner exports.
    pub(crate) fn variables(self) -> (&'static str, &'static str) {
        match self {
            Self::Consumer => ("LASH_E2E_CONSUMER_BIN", "LASH_E2E_CONSUMER_SHA256"),
            Self::Workbench => ("LASH_WORKBENCH_E2E_BIN", "LASH_WORKBENCH_E2E_SHA256"),
        }
    }

    pub(crate) fn role(self) -> &'static str {
        match self {
            Self::Consumer => "consumer",
            Self::Workbench => "workbench_e2e",
        }
    }
}

/// How the case boots one node.
#[derive(Clone, Debug, Default)]
pub struct NodeOptions {
    /// The case fixture the host reads, if any.
    pub fixture: Option<PathBuf>,
    /// The commit cuts the host's ledger holds at.
    pub cuts: Vec<Value>,
    /// The database URL the node connects through, when it is not the
    /// case's own (a proxy's).
    pub database_url: Option<String>,
    /// Further host environment.
    pub env: Vec<(String, String)>,
}

/// A running (or killed) host process.
pub struct Node {
    pub name: String,
    pub host: Host,
    pub url: String,
    pub(crate) child: Option<tokio::process::Child>,
    pub(crate) pid: u32,
    pub(crate) killed: bool,
    pub(crate) stdout: PathBuf,
    pub ledger: PathBuf,
    client: reqwest::Client,
    /// The case's deadline, which bounds every request to the node.
    deadline: Instant,
}

impl Node {
    #[expect(
        clippy::too_many_arguments,
        reason = "a boot is its host, name, binary, environment, case directory, boot number, ledger and deadline"
    )]
    pub(crate) async fn spawn(
        host: Host,
        name: &str,
        binary: &Path,
        env: Vec<(String, String)>,
        dir: &Path,
        boot: usize,
        ledger: PathBuf,
        deadline: Instant,
    ) -> Result<Self> {
        let port = free_port()?;
        let url = format!("http://127.0.0.1:{port}");
        let stdout = dir.join(format!("{name}-{boot}.stdout"));
        let stderr = dir.join(format!("{name}-{boot}.stderr"));
        let mut command = tokio::process::Command::new(binary);
        command
            .envs(env)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&stdout)?)
            .stderr(std::fs::File::create(&stderr)?)
            .kill_on_drop(true);
        match host {
            Host::Consumer => {
                command.env("E2E_CONSUMER_ADDR", format!("127.0.0.1:{port}"));
            }
            Host::Workbench => {
                command.env("AGENT_WORKBENCH_ADDR", format!("127.0.0.1:{port}"));
            }
        }
        let child = command
            .spawn()
            .with_context(|| format!("boot node {name} from {}", binary.display()))?;
        let pid = child.id().context("the node exited at boot")?;
        let mut node = Self {
            name: name.to_owned(),
            host,
            url,
            child: Some(child),
            pid,
            killed: false,
            stdout,
            ledger,
            client: reqwest::Client::builder().no_proxy().build()?,
            deadline,
        };
        node.ready(deadline).await?;
        Ok(node)
    }

    /// Wait until the node answers its health route.
    async fn ready(&mut self, deadline: Instant) -> Result<()> {
        loop {
            if let Some(child) = self.child.as_mut()
                && let Some(status) = child.try_wait()?
            {
                bail!("node {} exited before it served: {status}", self.name);
            }
            if let Ok(response) = self
                .client
                .get(format!("{}/healthz", self.url))
                .send()
                .await
                && response.status().is_success()
            {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "node {} did not serve by the deadline",
                self.name
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// The boot's process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether the boot still runs (as far as the case knows).
    #[must_use]
    pub fn live(&self) -> bool {
        self.child.is_some()
    }

    /// Freeze the boot (SIGSTOP): it holds its sockets and runs nothing.
    ///
    /// # Errors
    ///
    /// The boot is not live or cannot be signalled.
    pub fn freeze(&self) -> Result<()> {
        ensure!(self.child.is_some(), "boot {} is not live", self.pid);
        signal(self.pid, libc::SIGSTOP)
    }

    /// Resume a frozen boot (SIGCONT).
    ///
    /// # Errors
    ///
    /// The boot is not live or cannot be signalled.
    pub fn thaw(&self) -> Result<()> {
        ensure!(self.child.is_some(), "boot {} is not live", self.pid);
        signal(self.pid, libc::SIGCONT)
    }

    /// POST `body` to `path`, answering the JSON reply.
    ///
    /// # Errors
    ///
    /// The request fails or answers an error status.
    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let response = self
            .client
            .post(format!("{}{path}", self.url))
            .json(body)
            .send();
        let response = self
            .bounded(response)
            .await
            .with_context(|| format!("POST {path} on {}", self.name))?;
        reply(response, path).await
    }

    /// GET `path`, answering the JSON reply.
    ///
    /// # Errors
    ///
    /// The request fails or answers an error status.
    pub async fn get(&self, path: &str) -> Result<Value> {
        let response = self.client.get(format!("{}{path}", self.url)).send();
        let response = self
            .bounded(response)
            .await
            .with_context(|| format!("GET {path} on {}", self.name))?;
        reply(response, path).await
    }

    /// GET `path`, answering the raw body and its content type.
    ///
    /// # Errors
    ///
    /// The request fails or answers an error status.
    pub async fn bytes(&self, path: &str) -> Result<(Vec<u8>, String)> {
        let response = self.client.get(format!("{}{path}", self.url)).send();
        let response = self
            .bounded(response)
            .await
            .with_context(|| format!("GET {path} on {}", self.name))?;
        let status = response.status();
        let media = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = response.bytes().await?.to_vec();
        ensure!(status.is_success(), "{path} answered {status}");
        Ok((body, media))
    }

    /// DELETE `path`, answering the JSON reply.
    ///
    /// # Errors
    ///
    /// The request fails or answers an error status.
    pub async fn delete(&self, path: &str) -> Result<Value> {
        let response = self.client.delete(format!("{}{path}", self.url)).send();
        let response = self
            .bounded(response)
            .await
            .with_context(|| format!("DELETE {path} on {}", self.name))?;
        reply(response, path).await
    }

    /// Await `request`'s response by the case's deadline.
    async fn bounded(
        &self,
        request: impl Future<Output = reqwest::Result<reqwest::Response>>,
    ) -> Result<reqwest::Response> {
        Ok(tokio::time::timeout_at(self.deadline.into(), request)
            .await
            .context("no answer by the case deadline")??)
    }

    /// The node's commit ledger, as far as it has been written.
    ///
    /// # Errors
    ///
    /// A ledger line that is not JSON.
    pub fn commits(&self) -> Result<Vec<Value>> {
        crate::read_jsonl(&self.ledger)
    }

    /// SIGKILL the node and reap it.
    ///
    /// # Errors
    ///
    /// The process cannot be signalled or reaped.
    pub async fn kill(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            child.kill().await.context("SIGKILL the node")?;
            self.killed = true;
        }
        Ok(())
    }

    /// Stop the node cleanly (SIGTERM) and answer whether it shut down.
    ///
    /// # Errors
    ///
    /// The process cannot be signalled or does not exit by the deadline.
    pub async fn stop(&mut self, deadline: Instant) -> Result<String> {
        let Some(mut child) = self.child.take() else {
            return Ok("killed".to_owned());
        };
        if child.try_wait()?.is_none() {
            terminate(self.pid)?;
        }
        let status = tokio::time::timeout_at(deadline.into(), child.wait())
            .await
            .with_context(|| format!("node {} did not stop by the deadline", self.name))??;
        ensure!(status.success(), "node {} stopped with {status}", self.name);
        let stdout = std::fs::read_to_string(&self.stdout).unwrap_or_default();
        let marker = stdout
            .lines()
            .find(|line| line.starts_with("CONSUMER_SHUTDOWN") || line.contains("shutdown"))
            .unwrap_or("exited 0");
        Ok(marker.to_owned())
    }
}

/// Send SIGTERM to `pid`, this case's own unreaped child.
fn terminate(pid: u32) -> Result<()> {
    signal(pid, libc::SIGTERM)
}

/// Send `signal` to `pid`, this case's own unreaped child.
#[expect(
    unsafe_code,
    reason = "a clean host stop is SIGTERM and a frozen host is SIGSTOP, and kill(2) through libc is the narrowest FFI for them; the child is the case's own and not yet reaped"
)]
fn signal(pid: u32, signal: libc::c_int) -> Result<()> {
    let pid = i32::try_from(pid)?;
    // SAFETY: kill(2) has no memory-safety contract.
    let signalled = unsafe { libc::kill(pid, signal) };
    ensure!(
        signalled == 0,
        "signal {signal} to {pid}: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

async fn reply(response: reqwest::Response, path: &str) -> Result<Value> {
    let status = response.status();
    let text = response.text().await?;
    ensure!(status.is_success(), "{path} answered {status}: {text}");
    Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

/// A loopback port the OS reports free.
pub fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}
