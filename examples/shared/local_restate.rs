//! An example host's Restate deployment: the zero-infra effect engine
//! (ADR 0104 §4) on a local `restate-server`.
//!
//! A host builds `lash-restate`'s engine over its store set, serves the
//! engine's endpoint on a loopback address and registers that address with
//! the server's admin API. The server then drives every turn and process in
//! the endpoint's handlers; the host only sends (D5).
//!
//! Two ways to reach a server:
//!
//! * [`LocalRestate::from_env`] reads the addresses a launcher hands the host
//!   (`scripts/ci/with-service.sh restate`, or a dev script that keeps a
//!   server running beside a long-lived host).
//! * [`LocalRestateServer::shared`] starts a server this process's cores
//!   share, and stops it once the last of them lets go.
//!
//! Several cores share one server by namespace (ADR 0111): each core's engine
//! binds lash's services under its own namespace, and the server refuses a
//! registration over names another core's deployment holds. A host that runs
//! several independent cores at once takes a namespace for each
//! ([`LocalRestateServer::core`], [`LocalRestate::in_namespace`]).
//!
//! Each example includes this file with `#[path]` and uses the part it needs.

#![allow(
    dead_code,
    reason = "each example includes this file and uses part of it"
)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// The environment variable naming the `restate-server` binary
/// [`LocalRestateServer::shared`] starts; `restate-server` on `PATH` otherwise.
/// Launch scripts set it to `python3 scripts/ci/restate_suite.py server-path`,
/// the pinned release binary.
pub(crate) const RESTATE_SERVER_BIN_ENV: &str = "LASH_RESTATE_SERVER_BIN";

/// How long a spawned server has to answer its health endpoints.
const SERVER_READY: Duration = Duration::from_secs(60);

/// The addresses of a local `restate-server`, and the namespace and authority
/// a core's deployment there binds lash's services under.
#[derive(Clone, Debug)]
pub(crate) struct LocalRestate {
    pub(crate) ingress_url: String,
    pub(crate) admin_url: String,
    pub(crate) authority: lash::restate::RestateAuthorityId,
    pub(crate) namespace: lash::restate::RestateNamespace,
}

impl LocalRestate {
    /// Read `RESTATE_INGRESS_URL`, `RESTATE_ADMIN_URL` and
    /// `RESTATE_AUTHORITY_ID`; a host started without a server is refused,
    /// with `launcher` named as the way to start one.
    pub(crate) fn from_env(launcher: &str) -> Result<Self> {
        let read = |name: &str| {
            std::env::var(name)
                .with_context(|| format!("{name} is unset: start this host with {launcher}"))
        };
        Ok(Self {
            ingress_url: read("RESTATE_INGRESS_URL")?,
            admin_url: read("RESTATE_ADMIN_URL")?,
            authority: lash::restate::RestateAuthorityId::new(read("RESTATE_AUTHORITY_ID")?)
                .map_err(|error| anyhow::anyhow!("RESTATE_AUTHORITY_ID: {error}"))?,
            namespace: lash::restate::RestateNamespace::default(),
        })
    }

    /// This server, for a core of its own in `namespace`: the core's
    /// authority is this one's, qualified by the namespace.
    pub(crate) fn in_namespace(&self, namespace: &str) -> Result<Self> {
        let namespace = namespace
            .parse::<lash::restate::RestateNamespace>()
            .map_err(|error| anyhow::anyhow!("Restate namespace `{namespace}`: {error}"))?;
        let authority = lash::restate::RestateAuthorityId::new(format!(
            "{}.{namespace}",
            self.authority.binding_id()
        ))
        .map_err(|error| anyhow::anyhow!("Restate authority for {namespace}: {error}"))?;
        Ok(Self {
            ingress_url: self.ingress_url.clone(),
            admin_url: self.admin_url.clone(),
            authority,
            namespace,
        })
    }

    /// The engine over `stores`, reaching this server.
    pub(crate) fn engine(
        &self,
        stores: Arc<dyn lash::StoreSet>,
    ) -> Arc<lash::restate::RestateEngine> {
        Arc::new(lash::restate::RestateEngine::new(
            stores,
            lash::restate::config(
                self.ingress_url.clone(),
                self.admin_url.clone(),
                self.authority.clone(),
            )
            .with_namespace(self.namespace.clone()),
        ))
    }

    /// Serve `engine`'s `endpoint` on a free loopback port and register it
    /// with the server. The deployment serves until the returned handle drops.
    pub(crate) async fn serve(
        &self,
        engine: &lash::restate::RestateEngine,
        endpoint: restate_sdk::endpoint::Endpoint,
    ) -> Result<LocalDeployment> {
        self.serve_at(engine, SocketAddr::from(([127, 0, 0, 1], 0)), endpoint)
            .await
    }

    /// Serve `engine`'s `endpoint` at `addr` and register it with the server,
    /// which refuses it when another deployment holds its namespace's names.
    /// The deployment serves until the returned handle drops.
    ///
    /// Restate pins an in-flight invocation to the deployment it started on,
    /// so a host that restarts against a server that outlives it serves the
    /// same `addr` again.
    pub(crate) async fn serve_at(
        &self,
        engine: &lash::restate::RestateEngine,
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
            lash::restate::serve_endpoint(listener, endpoint, async move {
                let _ = stopped.await;
            })
            .await;
        });
        let deployment = LocalDeployment {
            addr: local,
            stop: Some(stop),
            serving,
        };
        engine
            .register_deployment(&uri)
            .await
            .with_context(|| format!("register the Restate deployment at {uri}"))?;
        Ok(deployment)
    }
}

/// A served, registered endpoint. Dropping it stops serving.
pub(crate) struct LocalDeployment {
    addr: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    serving: tokio::task::JoinHandle<()>,
}

impl LocalDeployment {
    /// The address the endpoint serves on.
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

/// A `restate-server` this process started, on free loopback ports and a
/// scratch data directory, shared by the process's cores. Dropping the last
/// handle kills the server and removes the directory.
pub(crate) struct LocalRestateServer {
    restate: LocalRestate,
    cores: AtomicU64,
    child: Child,
    base_dir: PathBuf,
}

impl LocalRestateServer {
    /// The server this process's cores share, started labelled `label` if no
    /// core holds it. It stops once the last handle drops.
    pub(crate) async fn shared(label: &str) -> Result<Arc<Self>> {
        static SHARED: tokio::sync::Mutex<Weak<LocalRestateServer>> =
            tokio::sync::Mutex::const_new(Weak::new());
        let mut shared = SHARED.lock().await;
        if let Some(server) = shared.upgrade() {
            return Ok(server);
        }
        let server = Arc::new(Self::spawn(label).await?);
        *shared = Arc::downgrade(&server);
        Ok(server)
    }

    /// This server for a new core labelled `label`: a namespace of its own,
    /// `label` and an ordinal, so concurrent cores never share lash's names.
    pub(crate) fn core(&self, label: &str) -> Result<LocalRestate> {
        let ordinal = self.cores.fetch_add(1, Ordering::Relaxed);
        self.restate.in_namespace(&format!("{label}-{ordinal}"))
    }

    /// Start a server labelled `label` and wait until it answers. The binary
    /// is [`RESTATE_SERVER_BIN_ENV`]'s, or `restate-server` on `PATH`.
    async fn spawn(label: &str) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
        let run = format!("{label}-{}-{ordinal}", std::process::id());
        let binary = std::env::var_os(RESTATE_SERVER_BIN_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("restate-server"));
        let base_dir = std::env::temp_dir().join(format!("lash-restate-{run}"));
        std::fs::create_dir_all(&base_dir)
            .with_context(|| format!("create {}", base_dir.display()))?;
        let log_path = base_dir.join("restate-server.log");
        let log = std::fs::File::create(&log_path)
            .with_context(|| format!("create {}", log_path.display()))?;
        let [ingress, admin, node] = free_ports()?;
        let mut command = Command::new(&binary);
        command
            .arg("--no-logo")
            .env("RESTATE_DEFAULT_NUM_PARTITIONS", "1")
            .env("RESTATE_ROCKSDB_TOTAL_MEMORY_SIZE", "256MB")
            .env("RESTATE_LOG_FILTER", "warn,restate=info")
            .env("RESTATE_LOG_FORMAT", "compact")
            .env("RESTATE_LOG_DISABLE_ANSI_CODES", "true")
            .env("RESTATE_LISTEN_MODE", "tcp")
            .env("RESTATE_BIND_IP", "127.0.0.1")
            .env("RESTATE_BASE_DIR", &base_dir)
            .env("RESTATE_NODE_NAME", "n1")
            .env("RESTATE_CLUSTER_NAME", format!("lash-{run}"))
            .env("RESTATE_BIND_PORT", node.to_string())
            .env(
                "RESTATE_INGRESS__BIND_ADDRESS",
                format!("127.0.0.1:{ingress}"),
            )
            .env("RESTATE_ADMIN__BIND_ADDRESS", format!("127.0.0.1:{admin}"))
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        // The server reaches the endpoint on loopback directly.
        for proxy in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
        ] {
            command.env_remove(proxy);
        }
        let child = command.spawn().with_context(|| {
            format!(
                "start {} (set {RESTATE_SERVER_BIN_ENV} to the pinned binary: \
                 `python3 scripts/ci/restate_suite.py server-path`)",
                binary.display()
            )
        })?;
        let mut server = Self {
            restate: LocalRestate {
                ingress_url: format!("http://127.0.0.1:{ingress}"),
                admin_url: format!("http://127.0.0.1:{admin}"),
                authority: lash::restate::RestateAuthorityId::new(format!("{label}:{run}"))
                    .map_err(|error| anyhow::anyhow!("Restate authority for {run}: {error}"))?,
                namespace: lash::restate::RestateNamespace::default(),
            },
            cores: AtomicU64::new(0),
            child,
            base_dir,
        };
        server.wait_ready(&log_path).await?;
        Ok(server)
    }

    /// The server's addresses and authority, in the default namespace.
    pub(crate) fn restate(&self) -> &LocalRestate {
        &self.restate
    }

    async fn wait_ready(&mut self, log_path: &std::path::Path) -> Result<()> {
        let client = reqwest::Client::new();
        let started = Instant::now();
        for url in [
            format!("{}/health", self.restate.admin_url),
            format!("{}/restate/health", self.restate.ingress_url),
        ] {
            loop {
                if client
                    .get(&url)
                    .send()
                    .await
                    .is_ok_and(|response| response.status().is_success())
                {
                    break;
                }
                if let Some(status) = self.child.try_wait()? {
                    bail!(
                        "restate-server exited with {status} before {url} answered:\n{}",
                        log_tail(log_path)
                    );
                }
                if started.elapsed() > SERVER_READY {
                    bail!(
                        "restate-server did not answer {url} within {SERVER_READY:?}:\n{}",
                        log_tail(log_path)
                    );
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        Ok(())
    }
}

impl Drop for LocalRestateServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.base_dir);
    }
}

/// Three loopback ports free at the time of asking.
fn free_ports() -> Result<[u16; 3]> {
    let listeners = [
        std::net::TcpListener::bind("127.0.0.1:0")?,
        std::net::TcpListener::bind("127.0.0.1:0")?,
        std::net::TcpListener::bind("127.0.0.1:0")?,
    ];
    let mut ports = [0; 3];
    for (port, listener) in ports.iter_mut().zip(&listeners) {
        *port = listener.local_addr()?.port();
    }
    Ok(ports)
}

fn log_tail(path: &std::path::Path) -> String {
    let log = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = log.lines().collect();
    lines[lines.len().saturating_sub(40)..].join("\n")
}
