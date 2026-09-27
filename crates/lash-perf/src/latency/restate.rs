//! A live `restate-server` for the latency gate.
//!
//! The gate always measures against a real server process — never the
//! in-process Restate test double — so the measured send path crosses the
//! same ingress, journal and deployment boundaries a release deployment
//! crosses. Two ways to reach one:
//!
//! * [`LocalRestate::from_env`] reads `RESTATE_INGRESS_URL` /
//!   `RESTATE_ADMIN_URL` — how `just latency-gate` hands it the server
//!   `scripts/ci/restate_suite.py serve` already runs.
//! * [`LocalRestateServer::spawn`] starts a private server for one run —
//!   the `--help`-style fallback for a bare `lash-perf latency` invocation.
//!   `LASH_RESTATE_SERVER_BIN` names the binary, `restate-server` on `PATH`
//!   otherwise.
//!
//! The authority id is the deployment's trust domain: the gate mints one per
//! run and hands the same value to the cross-worker child process, so host
//! and worker bind one Restate authority over one store set.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use lash::restate::{RestateAuthorityId, RestateEngine};

/// The environment variable naming the `restate-server` binary
/// [`LocalRestateServer::spawn`] starts.
pub(crate) const RESTATE_SERVER_BIN_ENV: &str = "LASH_RESTATE_SERVER_BIN";

/// How long a spawned server has to answer its health endpoints.
const SERVER_READY: Duration = Duration::from_secs(60);

/// The addresses of a live `restate-server` and this run's authority.
#[derive(Clone)]
pub(crate) struct LocalRestate {
    pub(crate) ingress_url: String,
    pub(crate) admin_url: String,
    pub(crate) authority: RestateAuthorityId,
    pub(crate) source: &'static str,
}

impl LocalRestate {
    /// The server `serve`-style launchers export, or a spawned private one.
    pub(crate) async fn discover(authority: &str) -> Result<(Self, Option<LocalRestateServer>)> {
        if let (Ok(ingress), Ok(admin)) = (
            std::env::var("RESTATE_INGRESS_URL"),
            std::env::var("RESTATE_ADMIN_URL"),
        ) {
            return Ok((
                Self {
                    ingress_url: ingress,
                    admin_url: admin,
                    authority: authority_id(authority)?,
                    source: "env",
                },
                None,
            ));
        }
        let server = LocalRestateServer::spawn("lash-perf-latency", authority).await?;
        let restate = server.restate.clone();
        Ok((restate, Some(server)))
    }

    /// The engine over `stores`, reaching this server.
    pub(crate) fn engine(&self, stores: Arc<dyn lash::StoreSet>) -> Arc<RestateEngine> {
        Arc::new(RestateEngine::new(
            stores,
            lash::restate::config(
                self.ingress_url.clone(),
                self.admin_url.clone(),
                self.authority.clone(),
            ),
        ))
    }

    /// Serve `endpoint` on a free loopback port and register it with the
    /// server. The deployment serves until the returned handle drops.
    pub(crate) async fn serve(
        &self,
        endpoint: restate_sdk::endpoint::Endpoint,
    ) -> Result<LocalDeployment> {
        self.serve_at(SocketAddr::from(([127, 0, 0, 1], 0)), endpoint)
            .await
    }

    /// Serve `endpoint` at `addr` and register it with the server.
    pub(crate) async fn serve_at(
        &self,
        addr: SocketAddr,
        endpoint: restate_sdk::endpoint::Endpoint,
    ) -> Result<LocalDeployment> {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind the Restate endpoint at {addr}"))?;
        let local = listener.local_addr()?;
        let uri = format!("http://{local}");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let serving = tokio::spawn(async move {
            restate_sdk::http_server::HttpServer::new(endpoint)
                .serve_with_cancel(listener, async move {
                    let _ = stopped.await;
                })
                .await;
        });
        register_deployment(&self.admin_url, &uri).await?;
        Ok(LocalDeployment {
            addr: local,
            stop: Some(stop),
            serving,
        })
    }
}

/// Register `uri` as this server's deployment, replacing any earlier one.
pub(crate) async fn register_deployment(admin_url: &str, uri: &str) -> Result<()> {
    let response = reqwest::Client::new()
        .post(format!("{}/deployments", admin_url.trim_end_matches('/')))
        .json(&serde_json::json!({ "uri": uri, "force": true }))
        .send()
        .await
        .context("register the endpoint with the Restate admin API")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("Restate refused the deployment at {uri}: {status} {body}");
    }
    Ok(())
}

/// A served, registered endpoint. Dropping it stops serving.
pub(crate) struct LocalDeployment {
    addr: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    serving: tokio::task::JoinHandle<()>,
}

impl LocalDeployment {
    /// The address the endpoint serves on.
    #[allow(dead_code)]
    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for LocalDeployment {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.serving.abort();
    }
}

/// A private `restate-server` this process started, on free loopback ports
/// and a scratch data directory. Dropping it kills the server and removes
/// the directory.
pub(crate) struct LocalRestateServer {
    restate: LocalRestate,
    child: Child,
    base_dir: PathBuf,
}

/// Mint an authority id for one gate run.
fn authority_id(seed: &str) -> Result<RestateAuthorityId> {
    RestateAuthorityId::new(seed).map_err(|error| anyhow::anyhow!("authority id: {error}"))
}

impl LocalRestateServer {
    /// Start a server labelled `label` and wait until it answers.
    pub(crate) async fn spawn(label: &str, authority: &str) -> Result<Self> {
        let run = format!("{label}-{}", std::process::id());
        let binary = std::env::var_os(RESTATE_SERVER_BIN_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("restate-server"));
        let base_dir = std::env::temp_dir().join(format!("lash-restate-{run}"));
        std::fs::create_dir_all(&base_dir)
            .with_context(|| format!("create {}", base_dir.display()))?;
        let log_path = base_dir.join("restate-server.log");
        let log = std::fs::File::create(&log_path)
            .with_context(|| format!("create {}", log_path.display()))?;
        let ingress = free_port()?;
        let admin = free_port()?;
        let node = free_port()?;
        let mut command = Command::new(&binary);
        command
            .env(
                "RESTATE_INGRESS__BIND_ADDRESS",
                format!("127.0.0.1:{ingress}"),
            )
            .env("RESTATE_ADMIN__BIND_ADDRESS", format!("127.0.0.1:{admin}"))
            .env("RESTATE_NODE__BIND_ADDRESS", format!("127.0.0.1:{node}"))
            .env("RESTATE_BASE_DIR", &base_dir)
            .env("RESTATE_CLUSTER_NAME", &run)
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .stdin(Stdio::null());
        let mut child = command
            .spawn()
            .with_context(|| format!("spawn {}", binary.display()))?;
        let deadline = Instant::now() + SERVER_READY;
        let ingress_url = format!("http://127.0.0.1:{ingress}");
        let admin_url = format!("http://127.0.0.1:{admin}");
        let client = reqwest::Client::new();
        loop {
            if Instant::now() > deadline {
                let _ = child.kill();
                bail!("restate-server did not answer within {SERVER_READY:?} (log: {log_path:?})");
            }
            let health = client
                .get(format!("{admin_url}/health"))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success());
            if health {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(Self {
            restate: LocalRestate {
                ingress_url,
                admin_url,
                authority: authority_id(authority)?,
                source: "spawned",
            },
            child,
            base_dir,
        })
    }
}

impl Drop for LocalRestateServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.base_dir);
    }
}

/// An unbound loopback port: bound, read, released. The latency gate runs
/// alone under its recipe's port block, so the small race is acceptable
/// here; `restate_suite.py serve` is the serialized path.
fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}
