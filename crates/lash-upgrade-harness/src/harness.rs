//! The test side: the two builds, the services they reach, and nodes as
//! child processes.
//!
//! Nothing here skips. A test that asks for the builds or the services and
//! does not find them fails with the variable it wanted, because a skipped
//! upgrade gate compares nothing. `just e2e-rolling` exports all of them.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::de::DeserializeOwned;

use crate::identity::{BuildIdentity, BuildLabel};
use crate::node::{MigrateReport, ServeReady, StoreSpec, TurnReport};

/// The N build's `lash-upgrade-node`.
pub const NODE_N_ENV: &str = "LASH_UPGRADE_NODE_N";
/// The N+1 (`synthetic-next`) build's `lash-upgrade-node`.
pub const NODE_NEXT_ENV: &str = "LASH_UPGRADE_NODE_NEXT";
/// The PostgreSQL database every PostgreSQL case migrates and serves.
pub const POSTGRES_URL_ENV: &str = "LASH_POSTGRES_DATABASE_URL";

/// How long a `serve` node has to register its deployment.
const SERVE_READY: Duration = Duration::from_secs(120);

fn required_env(name: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("{name} is unset: run this through `just e2e-rolling`"))
}

/// The live services a roll runs against.
#[derive(Clone, Debug)]
pub struct Services {
    pub ingress_url: String,
    pub admin_url: String,
    pub postgres_url: String,
}

impl Services {
    /// `RESTATE_INGRESS_URL`, `RESTATE_ADMIN_URL` and
    /// `LASH_POSTGRES_DATABASE_URL`, all required.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            ingress_url: required_env("RESTATE_INGRESS_URL")?,
            admin_url: required_env("RESTATE_ADMIN_URL")?,
            postgres_url: required_env(POSTGRES_URL_ENV)?,
        })
    }
}

/// The two builds.
#[derive(Clone, Debug)]
pub struct NodeBuilds {
    pub n: NodeBinary,
    pub next: NodeBinary,
}

impl NodeBuilds {
    /// The binaries [`NODE_N_ENV`] and [`NODE_NEXT_ENV`] name, each checked
    /// to report the build it is named as, and checked to be two builds.
    pub fn from_env() -> Result<Self> {
        let n = NodeBinary::at(PathBuf::from(required_env(NODE_N_ENV)?), BuildLabel::N)?;
        let next = NodeBinary::at(
            PathBuf::from(required_env(NODE_NEXT_ENV)?),
            BuildLabel::Next,
        )?;
        ensure!(
            n.identity.generation != next.identity.generation,
            "N and N+1 report one drain generation ({}): the synthetic-next build moved nothing \
             that G hashes",
            n.identity.generation
        );
        Ok(Self { n, next })
    }
}

/// One build's `lash-upgrade-node`.
#[derive(Clone, Debug)]
pub struct NodeBinary {
    path: PathBuf,
    identity: BuildIdentity,
}

impl NodeBinary {
    /// The binary at `path`, which must report itself as `label`.
    pub fn at(path: PathBuf, label: BuildLabel) -> Result<Self> {
        let output = Command::new(&path)
            .arg("version")
            .output()
            .with_context(|| format!("run {} version", path.display()))?;
        let identity: BuildIdentity = report(&path, "version", output)?;
        ensure!(
            identity.build == label,
            "{} reports build {}, expected {label}",
            path.display(),
            identity.build
        );
        Ok(Self { path, identity })
    }

    pub fn identity(&self) -> &BuildIdentity {
        &self.identity
    }

    pub fn label(&self) -> BuildLabel {
        self.identity.build
    }

    /// Provision or advance `case`'s store with this build's migrations.
    pub fn migrate(&self, case: &Case) -> Result<MigrateReport> {
        let output = Command::new(&self.path)
            .arg("migrate")
            .args(case.store_args())
            .output()
            .with_context(|| format!("run {} migrate", self.label()))?;
        report(&self.path, "migrate", output)
    }

    /// Serve a deployment of this build over `case`'s store, registered at
    /// a URI of its own. The node serves until the handle stops or drops.
    pub fn serve(&self, case: &Case) -> Result<ServingNode> {
        let ready_file = case.scratch.join(format!(
            "ready-{}-{}.json",
            self.label().as_str().replace('+', "p"),
            case.next_ordinal()
        ));
        let log_path = ready_file.with_extension("log");
        let log = std::fs::File::create(&log_path)
            .with_context(|| format!("create {}", log_path.display()))?;
        let child = Command::new(&self.path)
            .arg("serve")
            .args(case.store_args())
            .args(case.restate_args())
            .arg("--ready-file")
            .arg(&ready_file)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()
            .with_context(|| format!("spawn {} serve", self.label()))?;
        let mut node = ServingNode {
            child: Some(child),
            ready: None,
            log_path,
        };
        node.ready = Some(node.await_ready(&ready_file)?);
        Ok(node)
    }

    /// Send `message` to `session` as a host of this build and wait for the
    /// turn to settle Answered.
    pub fn turn(&self, case: &Case, session: &str, message: &str) -> Result<TurnReport> {
        let output = Command::new(&self.path)
            .arg("turn")
            .args(case.store_args())
            .args(case.restate_args())
            .args(["--session", session, "--message", message])
            .output()
            .with_context(|| format!("run {} turn", self.label()))?;
        report(&self.path, "turn", output)
    }
}

/// Decode a command's one-line JSON report, or fail with its stderr.
fn report<T: DeserializeOwned>(
    path: &Path,
    command: &str,
    output: std::process::Output,
) -> Result<T> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        bail!(
            "{} {command} failed ({}): {}",
            path.display(),
            output.status,
            stderr.trim()
        );
    }
    let line = stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| anyhow!("{} {command} printed no report", path.display()))?;
    serde_json::from_str(line)
        .with_context(|| format!("decode the {command} report of {}: {line}", path.display()))
}

/// A running `serve` node. Dropping it kills the process.
pub struct ServingNode {
    child: Option<Child>,
    ready: Option<ServeReady>,
    log_path: PathBuf,
}

impl ServingNode {
    /// What the node registered.
    pub fn ready(&self) -> Option<&ServeReady> {
        self.ready.as_ref()
    }

    fn await_ready(&mut self, ready_file: &Path) -> Result<ServeReady> {
        let deadline = Instant::now() + SERVE_READY;
        loop {
            if let Ok(bytes) = std::fs::read(ready_file) {
                return serde_json::from_slice(&bytes)
                    .with_context(|| format!("decode {}", ready_file.display()));
            }
            if let Some(child) = self.child.as_mut()
                && let Some(status) = child.try_wait()?
            {
                bail!(
                    "the serve node exited {status} before registering: {}",
                    self.log_tail()
                );
            }
            if Instant::now() > deadline {
                bail!(
                    "the serve node did not register within {SERVE_READY:?}: {}",
                    self.log_tail()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Kill the node, as a pod dies, and confirm it did not exit on its own
    /// first: a node that died by itself refused something.
    pub fn stop(mut self) -> Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        if let Some(status) = child.try_wait()? {
            bail!(
                "the serve node had already exited {status}: {}",
                self.log_tail()
            );
        }
        child.kill().context("kill the serve node")?;
        child.wait().context("reap the serve node")?;
        Ok(())
    }

    fn log_tail(&self) -> String {
        let log = std::fs::read_to_string(&self.log_path).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        let start = lines.len().saturating_sub(40);
        format!(
            "{} (log {})",
            lines[start..].join("\n"),
            self.log_path.display()
        )
    }
}

impl Drop for ServingNode {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// One roll: a store, a scratch directory, and the Restate authority and
/// namespace every build of the roll shares.
pub struct Case {
    pub name: String,
    store: StoreSpec,
    scratch: PathBuf,
    services: Services,
    authority: String,
    namespace: String,
    token: String,
    ordinal: std::cell::Cell<u32>,
}

impl Case {
    /// A roll over PostgreSQL, isolated under `name` on the shared server.
    pub fn postgres(name: &str, services: &Services, scratch: &Path) -> Result<Self> {
        Self::new(
            name,
            StoreSpec::Postgres(services.postgres_url.clone()),
            services,
            scratch,
        )
    }

    /// A roll over a fresh SQLite store directory under `scratch`.
    pub fn sqlite(name: &str, services: &Services, scratch: &Path) -> Result<Self> {
        let dir = scratch.join(name).join("stores");
        Self::new(name, StoreSpec::Sqlite(dir), services, scratch)
    }

    fn new(name: &str, store: StoreSpec, services: &Services, scratch: &Path) -> Result<Self> {
        let scratch = scratch.join(name);
        std::fs::create_dir_all(&scratch)
            .with_context(|| format!("create {}", scratch.display()))?;
        let token = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis())
                .unwrap_or_default()
        );
        Ok(Self {
            name: name.to_string(),
            store,
            scratch,
            services: services.clone(),
            authority: format!("lash-upgrade-{name}-{token}"),
            namespace: format!("up-{name}-{}", std::process::id()),
            token,
            ordinal: std::cell::Cell::new(0),
        })
    }

    /// A session id unique to this run, so a reused database never hands a
    /// run the session a previous run wrote.
    pub fn session_id(&self, prefix: &str) -> String {
        format!("{prefix}-{}-{}", self.name, self.token)
    }

    fn next_ordinal(&self) -> u32 {
        let ordinal = self.ordinal.get();
        self.ordinal.set(ordinal + 1);
        ordinal
    }

    fn store_args(&self) -> Vec<String> {
        vec![
            "--store".to_string(),
            self.store.to_string(),
            "--data-dir".to_string(),
            self.scratch.join("data").display().to_string(),
        ]
    }

    fn restate_args(&self) -> Vec<String> {
        vec![
            "--ingress-url".to_string(),
            self.services.ingress_url.clone(),
            "--admin-url".to_string(),
            self.services.admin_url.clone(),
            "--authority".to_string(),
            self.authority.clone(),
            "--namespace".to_string(),
            self.namespace.clone(),
        ]
    }
}
