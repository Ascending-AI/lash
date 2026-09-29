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
use lash_restate::VersionRange;
use serde::de::DeserializeOwned;

use crate::identity::BuildLabel;
use crate::node::objects::{CallReport, SweepLine, TargetKind};
use crate::node::process::{HarnessOpReport, ProcessStatusReport};
use crate::node::provider::{self, EffectRecord};
use crate::node::remote::ClientReport;
use crate::node::{ProbeReport, RegisterReport, ServeReady, StoreSpec, TurnReport};
use crate::restate_view::RestateView;

/// The N build's `lash-upgrade-node`.
pub const NODE_N_ENV: &str = "LASH_UPGRADE_NODE_N";
/// The N+1 (`synthetic-next`) build's `lash-upgrade-node`.
pub const NODE_NEXT_ENV: &str = "LASH_UPGRADE_NODE_NEXT";
/// The Bazel-built N operator binary.
pub const LASHCTL_N_ENV: &str = "LASH_UPGRADE_LASHCTL_N";
/// The Bazel-built synthetic-next operator binary.
pub const LASHCTL_NEXT_ENV: &str = "LASH_UPGRADE_LASHCTL_NEXT";
/// The PostgreSQL database every PostgreSQL case migrates and serves.
pub const POSTGRES_URL_ENV: &str = "LASH_POSTGRES_DATABASE_URL";

/// How long a `serve` node has to register its deployment.
const SERVE_READY: Duration = Duration::from_secs(120);

/// How long a leg waits for a condition it polls before it fails.
pub const WAIT: Duration = Duration::from_secs(120);

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

    /// A fresh database named `name` on the PostgreSQL server, and its URL.
    /// A leg that moves the fleet epoch owns its database, because `F` is
    /// one row per database.
    pub fn fresh_postgres_database(&self, name: &str) -> Result<String> {
        block_on(self.create_postgres_database(name))
    }

    /// [`fresh_postgres_database`](Self::fresh_postgres_database) from
    /// async code.
    pub async fn create_postgres_database(&self, name: &str) -> Result<String> {
        let database = name.replace('-', "_");
        ensure!(
            database
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "`{name}` is not a plain database name"
        );
        let create = format!("CREATE DATABASE {database}");
        {
            use sqlx::Connection as _;
            let mut connection = sqlx::PgConnection::connect(&self.postgres_url)
                .await
                .with_context(|| format!("connect to {}", self.postgres_url))?;
            sqlx::query(&create)
                .execute(&mut connection)
                .await
                .with_context(|| create.clone())?;
            connection.close().await.ok();
        }
        let (server, query) = match self.postgres_url.split_once('?') {
            Some((server, query)) => (server, format!("?{query}")),
            None => (self.postgres_url.as_str(), String::new()),
        };
        let base = server.rsplit_once('/').map_or(server, |(base, _)| base);
        Ok(format!("{base}/{database}{query}"))
    }

    /// These services over a fresh PostgreSQL database of the leg's own, so
    /// no leg sees another's stamps or rows, whatever order they run in.
    pub async fn isolated(&self, leg: &str) -> Result<Self> {
        let database = format!(
            "phase_a_{leg}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis())
                .unwrap_or_default()
        );
        Ok(Self {
            postgres_url: self.create_postgres_database(&database).await?,
            ..self.clone()
        })
    }
}

/// Run `future` to completion on a runtime of its own: the legs are
/// synchronous, and each Restate or PostgreSQL read is one blocking step.
pub fn block_on<T>(future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build a runtime")?
        .block_on(future)
}

/// A loopback address no deployment on the Restate server is registered at.
///
/// Every leg shares one server, and a stopped node's deployment stays
/// registered. The OS hands a later node that node's port again, and the
/// registration guard refuses the URI, rightly, since it names another
/// deployment. So a node is given a port whose URI no deployment holds.
fn unregistered_address(case: &Case) -> Result<String> {
    let view = case.view()?;
    // On a thread of its own: a leg may call `serve` from inside a runtime.
    let deployments = std::thread::scope(|scope| {
        scope
            .spawn(|| block_on(view.deployments()))
            .join()
            .map_err(|_| anyhow!("listing the deployments panicked"))
    })??;
    let registered: std::collections::BTreeSet<String> = deployments
        .into_iter()
        .map(|deployment| deployment.endpoint.trim_end_matches('/').to_owned())
        .collect();
    // Ports already refused stay bound, so the OS never offers them again.
    let mut refused = Vec::new();
    for _ in 0..64 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").context("pick a port")?;
        let address = listener.local_addr()?;
        if !registered.contains(&format!("http://{address}")) {
            return Ok(address.to_string());
        }
        refused.push(listener);
    }
    bail!("every loopback port offered is registered to a deployment")
}

/// Poll `probe` until it answers `Some`, failing after [`WAIT`] with what
/// the leg was waiting for.
pub fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Result<Option<T>>) -> Result<T> {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = probe()? {
            return Ok(value);
        }
        if Instant::now() > deadline {
            bail!("timed out after {WAIT:?} waiting for {what}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// How a `serve` node starts.
#[derive(Clone, Debug, Default)]
pub struct ServeOptions {
    /// The address the endpoint listens on; when `None`, a loopback port
    /// whose URI no deployment is registered at. A node that comes back at a crashed node's address serves
    /// the invocations pinned to that node's deployment.
    pub bind: Option<String>,
    /// Serve without registering the endpoint.
    pub unregistered: bool,
    /// Let [`ServingNode::register_now`] register the node later, through
    /// its own engine.
    pub register_later: bool,
}

/// One direct call to a lash handler ([`crate::node::objects`]).
#[derive(Clone, Debug)]
pub struct CallSpec<'a> {
    pub service: &'a str,
    pub kind: TargetKind,
    pub key: &'a str,
    pub handler: &'a str,
    pub body: serde_json::Value,
    /// The wire range the call states; the calling build's own when `None`.
    pub wire: Option<VersionRange>,
}

impl<'a> CallSpec<'a> {
    /// A call to an object handler at the calling build's wire range.
    pub fn object(service: &'a str, key: &'a str, handler: &'a str) -> Self {
        Self {
            service,
            kind: TargetKind::Object,
            key,
            handler,
            body: serde_json::Value::Null,
            wire: None,
        }
    }

    /// A call to a workflow handler at the calling build's wire range.
    pub fn workflow(service: &'a str, key: &'a str, handler: &'a str) -> Self {
        Self {
            kind: TargetKind::Workflow,
            ..Self::object(service, key, handler)
        }
    }

    pub fn body(self, body: serde_json::Value) -> Self {
        Self { body, ..self }
    }

    pub fn wire(self, wire: VersionRange) -> Self {
        Self {
            wire: Some(wire),
            ..self
        }
    }
}

/// The two builds.
#[derive(Clone, Debug)]
pub struct NodeBuilds {
    pub n: NodeBinary,
    pub next: NodeBinary,
}

impl NodeBuilds {
    /// The binaries [`NODE_N_ENV`] and [`NODE_NEXT_ENV`] name.
    pub fn from_env() -> Result<Self> {
        let n = NodeBinary::at(PathBuf::from(required_env(NODE_N_ENV)?), BuildLabel::N);
        let next = NodeBinary::at(
            PathBuf::from(required_env(NODE_NEXT_ENV)?),
            BuildLabel::Next,
        );
        Ok(Self { n, next })
    }
}

/// One build's `lash-upgrade-node`.
#[derive(Clone, Debug)]
pub struct NodeBinary {
    path: PathBuf,
    label: BuildLabel,
}

impl NodeBinary {
    /// The binary at `path`; its ready file verifies the label after start.
    pub fn at(path: PathBuf, label: BuildLabel) -> Self {
        Self { path, label }
    }

    pub fn label(&self) -> BuildLabel {
        self.label
    }

    /// Open the store in this build's own process without registering a
    /// deployment. A requested session is read through the backend view.
    fn probe_report(&self, case: &Case, session: Option<&str>) -> Result<ProbeReport> {
        let mut command = Command::new(&self.path);
        command.arg("probe").args(case.store_args());
        if let Some(session) = session {
            command.args(["--session", session]);
        }
        let output = command
            .output()
            .with_context(|| format!("run {} probe", self.label()))?;
        let report: ProbeReport = report(&self.path, "probe", output)?;
        ensure!(
            report.build == self.label,
            "{} probed as {}",
            self.label,
            report.build
        );
        Ok(report)
    }

    /// Open this build's store in its own process and require admission.
    pub fn probe(&self, case: &Case, session: Option<&str>) -> Result<ProbeReport> {
        let report = self.probe_report(case, session)?;
        ensure!(
            report.refusal.is_none(),
            "{} refused {}: {:?}",
            self.label,
            case.name,
            report.refusal
        );
        Ok(report)
    }

    /// Open this build's store in its own process and return a typed refusal.
    pub fn probe_refusal(&self, case: &Case) -> Result<lash_core::compat::CompatRefusal> {
        let report = self.probe_report(case, None)?;
        report.refusal.ok_or_else(|| {
            anyhow!(
                "{} admitted {} when a compatibility refusal was required",
                self.label,
                case.name
            )
        })
    }

    /// Serve a deployment of this build over `case`'s store, registered at
    /// a URI of its own. The node serves until the handle stops or drops.
    pub fn serve(&self, case: &Case) -> Result<ServingNode> {
        self.serve_with(case, &ServeOptions::default())
    }

    /// [`serve`](Self::serve) as `options` ask.
    pub fn serve_with(&self, case: &Case, options: &ServeOptions) -> Result<ServingNode> {
        let ready_file = case.scratch.join(format!(
            "ready-{}-{}.json",
            self.label().as_str().replace('+', "p"),
            case.next_ordinal()
        ));
        let log_path = ready_file.with_extension("log");
        let log = std::fs::File::create(&log_path)
            .with_context(|| format!("create {}", log_path.display()))?;
        let mut command = Command::new(&self.path);
        command
            .arg("serve")
            .args(case.store_args())
            .args(case.restate_args())
            .arg("--ready-file")
            .arg(&ready_file)
            .arg("--effects-log")
            .arg(case.effects_log())
            .arg("--gate-dir")
            .arg(case.gate_dir());
        let bind = match &options.bind {
            Some(bind) => bind.clone(),
            None => unregistered_address(case)?,
        };
        command.args(["--bind", &bind]);
        if options.unregistered {
            command.arg("--no-register");
        }
        let register_trigger = options
            .register_later
            .then(|| ready_file.with_extension("register"));
        if let Some(trigger) = &register_trigger {
            command.arg("--register-when").arg(trigger);
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()
            .with_context(|| format!("spawn {} serve", self.label()))?;
        let mut node = ServingNode {
            child: Some(child),
            ready: None,
            log_path,
            register_trigger,
        };
        let ready = node.await_ready(&ready_file)?;
        ensure!(
            ready.build == self.label,
            "{} served as {}",
            self.label,
            ready.build
        );
        node.ready = Some(ready);
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

    /// [`turn`](Self::turn) with `attachment` sent beside `message` as a
    /// `text/plain` attachment, which the session stores and roots.
    pub fn turn_with_attachment(
        &self,
        case: &Case,
        session: &str,
        message: &str,
        attachment: &str,
    ) -> Result<TurnReport> {
        let output = Command::new(&self.path)
            .arg("turn")
            .args(case.store_args())
            .args(case.restate_args())
            .args(["--session", session, "--message", message])
            .args(["--attachment", attachment])
            .output()
            .with_context(|| format!("run {} turn", self.label()))?;
        report(&self.path, "turn", output)
    }

    /// Run one `retention` step (`publish`, `release`, `orphan`, `relay`,
    /// `maintain` or `inspect` and its arguments) as this build over
    /// `case`'s store, with no deployment serving.
    pub fn retention<T: DeserializeOwned>(&self, case: &Case, step: &[&str]) -> Result<T> {
        let output = Command::new(&self.path)
            .arg("retention")
            .args(case.store_args())
            .args(case.restate_args())
            .args(step)
            .output()
            .with_context(|| format!("run {} retention {step:?}", self.label()))?;
        report(&self.path, "retention", output)
    }

    /// Send `message` to `session` as a host of this build, in the
    /// background: the turn settles while the leg moves deployments.
    pub fn spawn_turn(&self, case: &Case, session: &str, message: &str) -> Result<PendingTurn> {
        let child = Command::new(&self.path)
            .arg("turn")
            .args(case.store_args())
            .args(case.restate_args())
            .args(["--session", session, "--message", message])
            .args(["--timeout-secs", "600"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn {} turn", self.label()))?;
        Ok(PendingTurn {
            path: self.path.clone(),
            child,
        })
    }

    /// Register `uri` as this build's deployment, through lash's own
    /// registration guard.
    pub fn register(&self, case: &Case, uri: &str) -> Result<RegisterReport> {
        let output = Command::new(&self.path)
            .arg("register")
            .args(case.store_args())
            .args(case.restate_args())
            .args(["--uri", uri])
            .output()
            .with_context(|| format!("run {} register", self.label()))?;
        report(&self.path, "register", output)
    }

    /// Call one of lash's own handlers as a caller of this build.
    pub fn call(&self, case: &Case, spec: &CallSpec<'_>) -> Result<CallReport> {
        let mut command = Command::new(&self.path);
        command
            .arg("call")
            .args(case.restate_args())
            .args(["--service", spec.service, "--key", spec.key])
            .args(["--handler", spec.handler])
            .args([
                "--kind",
                match spec.kind {
                    TargetKind::Object => "object",
                    TargetKind::Workflow => "workflow",
                },
            ])
            .arg("--body")
            .arg(spec.body.to_string());
        if let Some(wire) = spec.wire {
            command
                .arg("--wire")
                .arg(format!("{}..{}", wire.min(), wire.max()));
        }
        let output = command
            .output()
            .with_context(|| format!("run {} call", self.label()))?;
        report(&self.path, "call", output)
    }

    /// Start the signal-waiting process through this build's deployment,
    /// under `key`.
    pub fn start_process(&self, case: &Case, key: &str) -> Result<HarnessOpReport> {
        let output = Command::new(&self.path)
            .arg("process-start")
            .args(case.restate_args())
            .args(["--key", key])
            .output()
            .with_context(|| format!("run {} process-start", self.label()))?;
        report(&self.path, "process-start", output)
    }

    /// Signal `process` through this build's deployment.
    pub fn signal_process(
        &self,
        case: &Case,
        process: &str,
        signal_id: &str,
        payload: &serde_json::Value,
    ) -> Result<HarnessOpReport> {
        self.spawn_signal(case, process, signal_id, payload)?.wait()
    }

    /// [`signal_process`](Self::signal_process) in the background, so two
    /// builds can signal at once.
    pub fn spawn_signal(
        &self,
        case: &Case,
        process: &str,
        signal_id: &str,
        payload: &serde_json::Value,
    ) -> Result<PendingSignal> {
        let child = Command::new(&self.path)
            .arg("process-signal")
            .args(case.restate_args())
            .args(["--process", process, "--signal-id", signal_id])
            .arg("--payload")
            .arg(payload.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("run {} process-signal", self.label()))?;
        Ok(PendingSignal {
            path: self.path.clone(),
            child,
        })
    }

    /// Read `process` from the store, as this build does.
    pub fn process_status(&self, case: &Case, process: &str) -> Result<ProcessStatusReport> {
        let output = Command::new(&self.path)
            .arg("process-status")
            .args(case.store_args())
            .args(case.restate_args())
            .args(["--process", process])
            .output()
            .with_context(|| format!("run {} process-status", self.label()))?;
        report(&self.path, "process-status", output)
    }

    /// Start this build's synthetic sweep in the background.
    pub fn spawn_sweep(&self, case: &Case) -> Result<Sweeper> {
        let mut child = Command::new(&self.path)
            .arg("sweep")
            .args(case.restate_args())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawn {} sweep", self.label()))?;
        let lines = std::io::BufReader::new(child.stdout.take().context("the sweep's stdout")?);
        Ok(Sweeper {
            child,
            lines: std::io::BufRead::lines(lines),
        })
    }

    /// Run one turn from this build's remote client through `peer`'s remote
    /// host, offering `offer` (this build's own range when `None`).
    pub fn remote_client(
        &self,
        case: &Case,
        peer: &NodeBinary,
        offer: Option<VersionRange>,
        session: &str,
        message: &str,
    ) -> Result<ClientReport> {
        let mut command = Command::new(&self.path);
        command
            .arg("remote-client")
            .args(case.store_args())
            .args(case.restate_args())
            .arg("--effects-log")
            .arg(case.effects_log())
            .arg("--gate-dir")
            .arg(case.gate_dir())
            .arg("--peer")
            .arg(&peer.path)
            .args(["--session", session, "--message", message]);
        if let Some(offer) = offer {
            command
                .arg("--offer")
                .arg(format!("{}..{}", offer.min(), offer.max()));
        }
        let output = command
            .output()
            .with_context(|| format!("run {} remote-client", self.label()))?;
        report(&self.path, "remote-client", output)
    }
}

/// A turn a host of one build is waiting on in the background.
pub struct PendingTurn {
    path: PathBuf,
    child: Child,
}

impl PendingTurn {
    /// Wait for the host to report the settled turn.
    pub fn wait(self) -> Result<TurnReport> {
        let output = self.child.wait_with_output().context("wait for the turn")?;
        report(&self.path, "turn", output)
    }
}

/// A `process-signal` running in the background.
pub struct PendingSignal {
    path: PathBuf,
    child: Child,
}

impl PendingSignal {
    /// Wait for the signal's report.
    pub fn wait(self) -> Result<HarnessOpReport> {
        let output = self
            .child
            .wait_with_output()
            .context("wait for the signal")?;
        report(&self.path, "process-signal", output)
    }
}

/// A running synthetic sweep, read one object at a time.
pub struct Sweeper {
    child: Child,
    lines: std::io::Lines<std::io::BufReader<std::process::ChildStdout>>,
}

impl Sweeper {
    /// The next object the sweep finished, or `None` once it ended.
    pub fn next_object(&mut self) -> Result<Option<SweepLine>> {
        match self.lines.next() {
            Some(line) => {
                let line = line.context("read the sweep")?;
                serde_json::from_str(&line)
                    .map(Some)
                    .with_context(|| format!("decode a sweep line: {line}"))
            }
            None => Ok(None),
        }
    }

    /// Kill the sweep where it stands, as a crash does.
    pub fn crash(mut self) -> Result<()> {
        self.child.kill().context("kill the sweep")?;
        self.child.wait().context("reap the sweep")?;
        Ok(())
    }

    /// Read the rest of the sweep and confirm it ended cleanly.
    pub fn finish(mut self) -> Result<Vec<SweepLine>> {
        let mut done = Vec::new();
        while let Some(line) = self.next_object()? {
            done.push(line);
        }
        let status = self.child.wait().context("reap the sweep")?;
        ensure!(status.success(), "the sweep exited {status}");
        Ok(done)
    }
}

/// The real operator binary, whose JSON output is kept in the test log.
pub struct Operator {
    path: PathBuf,
    postgres_url: String,
}

impl Operator {
    pub fn from_env(services: &Services, name: &str) -> Result<Self> {
        Ok(Self {
            path: PathBuf::from(required_env(name)?),
            postgres_url: services.postgres_url.clone(),
        })
    }

    /// The operator binary `name` names, over `case`'s own PostgreSQL
    /// database.
    pub fn for_case(case: &Case, name: &str) -> Result<Self> {
        let postgres_url = case
            .postgres_url()
            .ok_or_else(|| anyhow!("{} is not a PostgreSQL case", case.name))?
            .to_owned();
        Ok(Self {
            path: PathBuf::from(required_env(name)?),
            postgres_url,
        })
    }

    pub fn run(&self, verb: &str, generation: Option<&str>) -> Result<serde_json::Value> {
        self.run_args(&Self::words(verb, generation))
    }

    /// Run `verb` and answer its pinned exit code and its whole `--json`
    /// body, whether it succeeded or refused.
    pub fn answer(&self, verb: &str, generation: Option<&str>) -> Result<(i32, serde_json::Value)> {
        self.answer_args(&Self::words(verb, generation))
    }

    fn words<'a>(verb: &'a str, generation: Option<&'a str>) -> Vec<&'a str> {
        let mut words = vec![verb];
        words.extend(generation);
        words
    }

    /// Run the command `args` names and require it to succeed.
    pub fn run_args(&self, args: &[&str]) -> Result<serde_json::Value> {
        let (code, body) = self.answer_args(args)?;
        let verb = args.first().copied().unwrap_or_default();
        ensure!(code == 0, "lashctl {verb} failed (exit {code}): {body}");
        ensure!(
            body["error"].is_null(),
            "invalid lashctl {verb} result: {body}"
        );
        Ok(body["result"].clone())
    }

    /// Run the command `args` names and answer its pinned exit code and its
    /// whole `--json` body, whether it succeeded or refused.
    pub fn answer_args(&self, args: &[&str]) -> Result<(i32, serde_json::Value)> {
        let verb = args.first().copied().unwrap_or_default();
        let output = Command::new(&self.path)
            .args(args)
            .arg("--json")
            .env("LASH_POSTGRES_DATABASE_URL", &self.postgres_url)
            .output()
            .with_context(|| format!("run lashctl {verb}"))?;
        let body: serde_json::Value =
            serde_json::from_slice(&output.stdout).with_context(|| {
                format!(
                    "lashctl {verb} did not emit JSON: {}",
                    String::from_utf8_lossy(&output.stderr)
                )
            })?;
        println!("lashctl {} --json: {body}", args.join(" "));
        ensure!(
            body["schema_version"] == 1 && body["command"] == verb,
            "invalid lashctl {verb} result: {body}"
        );
        let code = output
            .status
            .code()
            .with_context(|| format!("lashctl {verb} ended by a signal"))?;
        Ok((code, body))
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
    register_trigger: Option<PathBuf>,
}

impl ServingNode {
    /// Register a node served with [`ServeOptions::register_later`] now,
    /// through its own engine and the registration guard.
    pub fn register_now(&self) -> Result<()> {
        let trigger = self
            .register_trigger
            .as_ref()
            .context("the node was not served to register later")?;
        let mut done = trigger.clone().into_os_string();
        done.push(".done");
        let done = PathBuf::from(done);
        std::fs::write(trigger, b"").with_context(|| format!("write {}", trigger.display()))?;
        let outcome: crate::node::RegisterWhenDone =
            wait_for("the node to register", || match std::fs::read(&done) {
                Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error).with_context(|| format!("read {}", done.display())),
            })?;
        match outcome.error {
            None => Ok(()),
            Some(error) => bail!("the node's registration failed: {error}"),
        }
    }

    /// What the node registered.
    pub fn ready(&self) -> Option<&ServeReady> {
        self.ready.as_ref()
    }

    /// The URI the node serves at.
    pub fn uri(&self) -> Result<&str> {
        self.ready
            .as_ref()
            .map(|ready| ready.uri.as_str())
            .ok_or_else(|| anyhow!("the node has not registered"))
    }

    /// The generation the node serves.
    pub fn generation(&self) -> Result<&str> {
        self.ready
            .as_ref()
            .map(|ready| ready.generation.as_str())
            .ok_or_else(|| anyhow!("the node has not registered"))
    }

    /// The `host:port` the node listens on, for a node that comes back in
    /// its place.
    pub fn bind(&self) -> Result<String> {
        let uri = self.uri()?;
        Ok(uri
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_owned())
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

    /// A roll over a fresh PostgreSQL database of its own, for a leg that
    /// moves the fleet epoch.
    pub fn postgres_database(name: &str, services: &Services, scratch: &Path) -> Result<Self> {
        let database = format!(
            "phase_a_{name}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis())
                .unwrap_or_default()
        );
        let url = services.fresh_postgres_database(&database)?;
        Self::new(name, StoreSpec::Postgres(url), services, scratch)
    }

    /// A roll over a fresh SQLite store directory under `scratch`.
    pub fn sqlite(name: &str, services: &Services, scratch: &Path) -> Result<Self> {
        let dir = scratch.join(name).join("stores");
        Self::new(name, StoreSpec::Sqlite(dir), services, scratch)
    }

    fn new(name: &str, store: StoreSpec, services: &Services, scratch: &Path) -> Result<Self> {
        let scratch = scratch.join(name);
        std::fs::create_dir_all(scratch.join("gates"))
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

    /// A case over the same store whose deployments bind under a namespace
    /// and authority of their own, so they never take this case's calls.
    pub fn beside(&self, name: &str) -> Result<Self> {
        let scratch = self.scratch.join(name);
        std::fs::create_dir_all(scratch.join("gates"))
            .with_context(|| format!("create {}", scratch.display()))?;
        Ok(Self {
            name: format!("{}-{name}", self.name),
            store: self.store.clone(),
            scratch,
            services: self.services.clone(),
            authority: format!("{}-{name}", self.authority),
            namespace: format!("{}-{name}", self.namespace),
            token: self.token.clone(),
            ordinal: std::cell::Cell::new(0),
        })
    }

    /// The case's PostgreSQL URL, when it is a PostgreSQL case.
    pub fn postgres_url(&self) -> Option<&str> {
        match &self.store {
            StoreSpec::Postgres(url) => Some(url),
            StoreSpec::Sqlite(_) => None,
        }
    }

    /// The case's SQLite store directory, when it is a SQLite case.
    pub fn sqlite_dir(&self) -> Option<&Path> {
        match &self.store {
            StoreSpec::Postgres(_) => None,
            StoreSpec::Sqlite(dir) => Some(dir),
        }
    }

    /// The Restate server, seen through the case's namespace.
    pub fn view(&self) -> Result<RestateView> {
        RestateView::new(&self.services.admin_url, &self.namespace)
    }

    /// The log every serving node of the case appends its model calls to.
    pub fn effects_log(&self) -> PathBuf {
        self.scratch.join("effects.jsonl")
    }

    /// Every model call a serving node of the case made.
    pub fn effects(&self) -> Result<Vec<EffectRecord>> {
        provider::read_effects(&self.effects_log())
    }

    /// The model calls whose message contains `marker`.
    pub fn effects_of(&self, marker: &str) -> Result<Vec<EffectRecord>> {
        Ok(self
            .effects()?
            .into_iter()
            .filter(|effect| effect.message.contains(marker))
            .collect())
    }

    /// Where held model calls wait for their release.
    pub fn gate_dir(&self) -> PathBuf {
        self.scratch.join("gates")
    }

    /// Wait until a model call holding at `gate` reached the provider, and
    /// answer the generation of the node that holds it.
    pub fn await_gate(&self, gate: &str) -> Result<String> {
        let reached = provider::reached_file(&self.gate_dir(), gate);
        wait_for(&format!("a model call to reach gate {gate}"), || {
            Ok(std::fs::read_to_string(&reached).ok())
        })
    }

    /// Release every model call held at `gate`.
    pub fn release(&self, gate: &str) -> Result<()> {
        let release = provider::release_file(&self.gate_dir(), gate);
        std::fs::write(&release, gate).with_context(|| format!("release gate {gate}"))
    }

    /// Retire `generation`: remove every deployment the server holds that
    /// serves its lanes, answering how many there were. The drain must have
    /// read drained first; this is the host's half of retirement.
    pub fn retire_generation(&self, generation: &str) -> Result<usize> {
        let view = self.view()?;
        block_on(async move {
            let retained = view.deployments_of_generation(generation).await?;
            for deployment in &retained {
                view.remove_deployment(&deployment.id).await?;
            }
            Ok(retained.len())
        })
    }

    /// The `lashctl finalize` arguments that retire `generation` against
    /// this case's Restate server.
    pub fn finalize_args<'a>(&'a self, generation: &'a str) -> [&'a str; 4] {
        [
            "finalize",
            generation,
            "--restate-admin-url",
            self.services.admin_url.as_str(),
        ]
    }

    /// The last step of `generation`'s drain: retire its deployments, then
    /// finalize with the operator binary `operator` (FIG-3800 B). Answers
    /// `lashctl finalize`'s result.
    pub fn retire_and_finalize(
        &self,
        operator: &Operator,
        generation: &str,
    ) -> Result<serde_json::Value> {
        self.retire_generation(generation)?;
        operator.run_args(&self.finalize_args(generation))
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
