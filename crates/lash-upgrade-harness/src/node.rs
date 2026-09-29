//! `lash-upgrade-node`: one Phase A build as a process.
//!
//! Every command prints one JSON object on stdout and exits non-zero with
//! the error on stderr when it fails, so the harness can tell a refusal
//! from a success without parsing logs.
//!
//! - `serve` opens a store, serves lash's Restate services over it on a
//!   fresh loopback port, registers that deployment and parks until it is
//!   killed. It writes the deployment's address to its ready file.
//! - `turn` is a host: it sends one input to a session through Restate and
//!   waits for the turn to settle, wherever it ran.
//! - `register` registers an endpoint URI as this build would, and reports
//!   the typed refusal of a URI another generation's deployment holds.
//! - `call` and `sweep` call lash's own handlers directly ([`objects`]).
//! - `remote-client` and `remote-host` speak the remote protocol between
//!   two builds ([`remote`]).
//! - `process-start`, `process-signal` and `process-status` start, signal
//!   and read a durable process ([`process`]).
//!
//! The scripted provider answers every model call with the serving build's
//! label and `G`, so a turn's reply names the build that drove it, and it
//! records and holds calls as [`provider`] describes.

pub mod objects;
pub mod process;
pub mod provider;
pub mod remote;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};

use crate::identity::BuildLabel;
use objects::{CallArgs, SweepArgs};
use process::{ProcessSignalArgs, ProcessStartArgs, ProcessStatusArgs};
use provider::ProviderArgs;
use remote::{RemoteClientArgs, RemoteHostArgs};

/// The node binary's command line.
#[derive(Debug, Parser)]
#[command(
    name = "lash-upgrade-node",
    about = "One Phase A build of lash (ADR 0115 §6)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open a store and optionally read one existing session without serving.
    Probe(ProbeArgs),
    /// Serve a Restate deployment over a store until killed.
    Serve(ServeArgs),
    /// Send one input to a session and wait for its turn to settle.
    Turn(TurnArgs),
    /// Register an endpoint URI as this build's deployment.
    Register(RegisterArgs),
    /// Call one of lash's own handlers directly.
    Call(CallArgs),
    /// Upgrade every effect group still at format 1 (the synthetic sweep).
    Sweep(SweepArgs),
    /// Serve one remote-protocol connection on stdin and stdout.
    RemoteHost(RemoteHostArgs),
    /// Run one turn through a peer build's remote host.
    RemoteClient(RemoteClientArgs),
    /// Start the signal-waiting process through this build's deployment.
    ProcessStart(ProcessStartArgs),
    /// Signal a process through this build's deployment.
    ProcessSignal(ProcessSignalArgs),
    /// Read a process: its lifecycle, its signals and its output.
    ProcessStatus(ProcessStatusArgs),
}

/// Which store a command opens.
#[derive(Clone, Debug, Args)]
pub struct StoreArgs {
    /// `postgres://…` for a PostgreSQL database, `sqlite:<dir>` for a SQLite
    /// store directory.
    #[arg(long)]
    pub store: StoreSpec,
    /// Node scratch: the PostgreSQL store set's attachment bytes.
    #[arg(long)]
    pub data_dir: PathBuf,
}

/// The Restate server a node reaches and the deployment it belongs to.
#[derive(Clone, Debug, Args)]
pub struct RestateArgs {
    #[arg(long, env = "RESTATE_INGRESS_URL")]
    pub ingress_url: String,
    #[arg(long, env = "RESTATE_ADMIN_URL")]
    pub admin_url: String,
    /// The trust domain every build of one roll shares.
    #[arg(long)]
    pub authority: String,
    /// The ADR 0111 namespace every build of one roll binds under.
    #[arg(long, default_value = "")]
    pub namespace: String,
}

#[derive(Clone, Debug, Args)]
pub struct ServeArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    #[command(flatten)]
    pub restate: RestateArgs,
    /// Where the endpoint listens; port 0 picks a fresh one, so every
    /// deployment gets a URI no other deployment holds (ADR 0115 §3.5).
    #[arg(long, default_value = "127.0.0.1:0")]
    pub bind: SocketAddr,
    /// Written with a [`ServeReady`] once the deployment is registered.
    #[arg(long)]
    pub ready_file: PathBuf,
    /// Serve without registering: the endpoint answers at its URI, and only
    /// an operator's registration, or a deployment already registered at
    /// that URI, routes invocations to it.
    #[arg(long)]
    pub no_register: bool,
    /// Register later, through this node's own engine and its registration
    /// guard, once this file exists; the outcome is written beside it, to
    /// the same path with `.done` appended. A node that opened the store
    /// before finalize registers after it, as an operator keeping it would.
    #[arg(long)]
    pub register_when: Option<PathBuf>,
    #[command(flatten)]
    pub provider: ProviderArgs,
}

#[derive(Clone, Debug, Args)]
pub struct RegisterArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    #[command(flatten)]
    pub restate: RestateArgs,
    /// The endpoint URI to register.
    #[arg(long)]
    pub uri: String,
}

#[derive(Clone, Debug, Args)]
pub struct TurnArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    #[command(flatten)]
    pub restate: RestateArgs,
    #[arg(long)]
    pub session: String,
    #[arg(long)]
    pub message: String,
    /// How long the turn may take to settle.
    #[arg(long, default_value_t = 120)]
    pub timeout_secs: u64,
}

#[derive(Clone, Debug, Args)]
pub struct ProbeArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// Read this session through the store's read-only session view.
    #[arg(long)]
    pub session: Option<String>,
}

/// A store a node opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreSpec {
    Postgres(String),
    Sqlite(PathBuf),
}

impl std::str::FromStr for StoreSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.starts_with("postgres://") || value.starts_with("postgresql://") {
            Ok(Self::Postgres(value.to_string()))
        } else if let Some(dir) = value.strip_prefix("sqlite:") {
            Ok(Self::Sqlite(PathBuf::from(dir)))
        } else {
            Err(format!(
                "`{value}` is not a store: pass `postgres://…` or `sqlite:<dir>`"
            ))
        }
    }
}

impl std::fmt::Display for StoreSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Postgres(url) => formatter.write_str(url),
            Self::Sqlite(dir) => write!(formatter, "sqlite:{}", dir.display()),
        }
    }
}

impl StoreSpec {
    /// The backend's name in reports.
    pub fn backend(&self) -> &'static str {
        match self {
            Self::Postgres(_) => "postgres",
            Self::Sqlite(_) => "sqlite",
        }
    }
}

/// What `serve` writes to its ready file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServeReady {
    pub build: BuildLabel,
    pub generation: String,
    /// The URI the deployment is registered at.
    pub uri: String,
}

/// The typed refusal of a registration at a URI that serves another
/// generation (ADR 0115 §3.5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointServesAnotherGeneration {
    pub uri: String,
    /// The generation whose names the held deployment serves.
    pub held: Option<String>,
    /// This build's generation.
    pub local: String,
}

/// What `register` reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterReport {
    pub build: BuildLabel,
    pub uri: String,
    /// `Ok` once registered; the typed refusal otherwise.
    pub registered: Result<(), EndpointServesAnotherGeneration>,
}

/// What `turn` reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnReport {
    /// The build that sent the input.
    pub host: BuildLabel,
    pub session: String,
    pub status: String,
    /// The assistant's reply: it names the build that drove the turn.
    pub reply: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeReport {
    pub build: BuildLabel,
    pub backend: String,
    pub session_present: Option<bool>,
    pub refusal: Option<lash_core::compat::CompatRefusal>,
}

/// The reply the scripted provider gives on `build` at `generation`.
pub fn served_by(build: BuildLabel, generation: &str) -> String {
    format!("served by {build} at generation {generation}")
}

/// Run one command and print its report.
pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Probe(args) => print(&probe(args).await?),
        Command::Serve(args) => serve(args).await,
        Command::Turn(args) => print(&turn(args).await?),
        Command::Register(args) => print(&register(args).await?),
        Command::Call(args) => print(&objects::call(args).await?),
        Command::Sweep(args) => objects::sweep(args).await.map(drop),
        Command::RemoteHost(args) => remote::host(args).await,
        Command::RemoteClient(args) => {
            print(&tokio::task::spawn_blocking(move || remote::client(args)).await??)
        }
        Command::ProcessStart(args) => print(&process::start(args).await?),
        Command::ProcessSignal(args) => print(&process::signal(args).await?),
        Command::ProcessStatus(args) => print(&process::status(args).await?),
    }
}

async fn probe(args: ProbeArgs) -> Result<ProbeReport> {
    let build = BuildLabel::current();
    let backend = args.store.store.backend().to_string();
    let stores = match open_stores(&args.store).await {
        Ok(stores) => stores,
        Err(error) => {
            if let Some(refusal) = error.chain().find_map(|cause| {
                match cause.downcast_ref::<lash_core::StoreError>() {
                    Some(lash_core::StoreError::Incompatible { refusal }) => Some(refusal.clone()),
                    _ => None,
                }
            }) {
                return Ok(ProbeReport {
                    build,
                    backend,
                    session_present: None,
                    refusal: Some(refusal),
                });
            }
            return Err(error);
        }
    };
    let session_present = if let Some(session) = args.session {
        let id = lash::SessionId::from(session);
        Some(
            stores
                .session_store_factory()
                .read_session(&id)
                .await?
                .is_some(),
        )
    } else {
        None
    };
    Ok(ProbeReport {
        build,
        backend,
        session_present,
        refusal: None,
    })
}

fn print(report: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string(report)?);
    Ok(())
}

async fn open_sqlite(dir: &Path) -> Result<lash::sqlite::SqliteStoreSet> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    lash::sqlite::SqliteStoreSet::open(dir)
        .await
        .with_context(|| format!("open the SQLite store set at {}", dir.display()))
}

async fn open_stores(args: &StoreArgs) -> Result<Arc<dyn lash::StoreSet>> {
    match &args.store {
        StoreSpec::Postgres(url) => {
            let storage = lash_postgres_store::PostgresStorage::connect(url)
                .await
                .context("open the PostgreSQL store")?;
            let attachments = args.data_dir.join("attachments");
            std::fs::create_dir_all(&attachments)
                .with_context(|| format!("create {}", attachments.display()))?;
            Ok(Arc::new(lash_postgres_store::PostgresStoreSet::new(
                &storage,
                Arc::new(lash::persistence::FileAttachmentStore::new(attachments)),
            )))
        }
        StoreSpec::Sqlite(dir) => Ok(Arc::new(open_sqlite(dir).await?)),
    }
}

fn engine(
    stores: Arc<dyn lash::StoreSet>,
    restate: &RestateArgs,
) -> Result<Arc<lash::restate::RestateEngine>> {
    let authority = lash::restate::RestateAuthorityId::new(&restate.authority)
        .map_err(|error| anyhow!("authority id: {error}"))?;
    let namespace = lash::restate::RestateNamespace::new(&restate.namespace)
        .map_err(|error| anyhow!("namespace: {error}"))?;
    Ok(Arc::new(lash::restate::RestateEngine::new(
        stores,
        lash::restate::config(
            restate.ingress_url.clone(),
            restate.admin_url.clone(),
            authority,
        )
        .with_namespace(namespace),
    )))
}

/// The arguments that open the same store in another process.
fn store_args(store: &StoreArgs) -> Vec<String> {
    vec![
        "--store".to_owned(),
        store.store.to_string(),
        "--data-dir".to_owned(),
        store.data_dir.display().to_string(),
    ]
}

/// The arguments that reach the same Restate deployment in another process.
fn restate_args(restate: &RestateArgs) -> Vec<String> {
    vec![
        "--ingress-url".to_owned(),
        restate.ingress_url.clone(),
        "--admin-url".to_owned(),
        restate.admin_url.clone(),
        "--authority".to_owned(),
        restate.authority.clone(),
        "--namespace".to_owned(),
        restate.namespace.clone(),
    ]
}

/// The model every node's sessions and processes name.
fn model() -> Result<lash::ModelSpec> {
    lash::ModelSpec::builder("upgrade-harness-model")
        .context_window_tokens(200_000)
        .build()
        .map_err(|error| anyhow!("model spec: {error}"))
}

/// Each build's recovery lease: N+1 outranks N, so the newest build leads
/// the drain's hand-over, and a lease whose holder died lapses in seconds.
fn recovery_lease() -> lash::RecoveryLeaseConfig {
    lash::RecoveryLeaseConfig {
        generation_rank: match BuildLabel::current() {
            BuildLabel::N => 0,
            BuildLabel::Next => 1,
        },
        timings: lash::RecoveryLeaseTimings {
            ttl: Duration::from_secs(3),
            renew_every: Duration::from_millis(500),
            renew_timeout: Duration::from_secs(2),
            trust_margin: Duration::from_millis(250),
            follower_retry: Duration::from_millis(250),
            follower_jitter: Duration::ZERO,
            min_tenure: Duration::ZERO,
        },
    }
}

/// The core every node builds: the scripted provider over `backend`, and
/// the Lashlang process engine over the store's artifacts. A host never
/// calls the provider; the node that drives a turn does, and records and
/// holds each call as `observed` asks.
fn core(backend: lash::Backend, observed: &ProviderArgs) -> Result<lash::LashCore> {
    let build = BuildLabel::current();
    let generation = lash::formats::build_generation().to_string();
    let reply = served_by(build, &generation);
    let observed = Arc::new(observed.clone());
    let provider = lash_core::testing::TestProvider::builder()
        .kind("upgrade-harness")
        .options(lash_core::provider::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::disabled(),
            ..lash_core::provider::ProviderOptions::default()
        })
        // Both builds serialize the same config, so a session one build
        // opened reads as the same provider on the other.
        .serialize_config(|| serde_json::json!({ "fixture": "upgrade-harness" }))
        .complete(move |request| {
            let reply = reply.clone();
            let observed = Arc::clone(&observed);
            let generation = generation.clone();
            let message = provider::newest_message(&request);
            async move {
                observed
                    .observe(build, &generation, &message)
                    .await
                    .map_err(|error| {
                        lash_core::llm::transport::LlmTransportError::new(format!(
                            "the scripted provider could not record its call: {error:#}"
                        ))
                    })?;
                Ok(scripted_reply(reply))
            }
        })
        .build();
    let artifacts = lashlang::LashlangArtifacts::of_backend(&backend);
    lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .provider(provider.into_handle())
        .model(model()?)
        .plugin(Arc::new(process::ProcessEnginePlugin(artifacts)))
        .recovery_lease(recovery_lease())
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "lash-upgrade-node",
            format!("{build}-{}", std::process::id()),
        ))
        .map_err(anyhow::Error::from)
}

fn scripted_reply(text: String) -> lash_core::llm::types::LlmResponse {
    use lash_core::llm::types::{LlmOutputPart, LlmResponse, LlmUsage};
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text,
            response_meta: None,
        }],
        usage: LlmUsage {
            input_tokens: 16,
            output_tokens: 8,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
        },
        terminal_reason: lash_core::LlmTerminalReason::Stop,
        terminal_diagnostic: None,
        provider_usage: None,
        request_body: None,
        http_summary: None,
        execution_evidence: None,
        generation_disposition: None,
        response_metadata: Default::default(),
        expose_thinking: None,
    }
}

/// A deployment of this build serving on a loopback port.
pub(crate) struct Serving {
    pub(crate) core: lash::LashCore,
    pub(crate) engine: Arc<lash::restate::RestateEngine>,
    pub(crate) ready: ServeReady,
    stop: tokio::sync::oneshot::Sender<()>,
    serving: tokio::task::JoinHandle<()>,
}

impl Serving {
    /// Open the store, serve lash's services on `bind` and, when `register`,
    /// register the endpoint's URI.
    pub(crate) async fn start(
        store: &StoreArgs,
        restate: &RestateArgs,
        provider: &ProviderArgs,
        bind: SocketAddr,
        register: bool,
    ) -> Result<Self> {
        let stores = open_stores(store).await?;
        let engine = engine(stores, restate)?;
        let backend = lash::Backend::new(engine.clone());
        let artifacts = lashlang::LashlangArtifacts::of_backend(&backend);
        let core = core(backend, provider)?;
        let worker = lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config()
                .context("process worker config")?,
        )
        .map_err(|error| anyhow!("build the process worker: {error}"))?;
        let endpoint = process::bind(
            engine.endpoint_builder(worker),
            &restate.namespace,
            process::HarnessProcesses {
                core: core.clone(),
                artifacts,
                authority: lash::restate::RestateAuthorityId::new(&restate.authority)
                    .map_err(|error| anyhow!("authority id: {error}"))?,
                namespace: lash::restate::RestateNamespace::new(&restate.namespace)
                    .map_err(|error| anyhow!("namespace: {error}"))?,
                model: model()?,
            },
        )?
        .build();
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .with_context(|| format!("bind the endpoint at {bind}"))?;
        let uri = format!("http://{}", listener.local_addr()?);
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let serving = tokio::spawn(async move {
            lash::restate::serve_endpoint(listener, endpoint, async move {
                let _ = stopped.await;
            })
            .await;
        });
        if register {
            engine
                .register_deployment(&uri)
                .await
                .with_context(|| format!("register the deployment at {uri}"))?;
        }
        let ready = ServeReady {
            build: BuildLabel::current(),
            generation: engine.build_generation().to_string(),
            uri,
        };
        Ok(Self {
            core,
            engine,
            ready,
            stop,
            serving,
        })
    }

    /// Stop serving. The core lives until the endpoint has stopped.
    pub(crate) async fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.serving.await;
        drop(self.core);
    }
}

async fn serve(args: ServeArgs) -> Result<()> {
    let serving = Serving::start(
        &args.store,
        &args.restate,
        &args.provider,
        args.bind,
        !args.no_register,
    )
    .await?;
    write_atomically(&args.ready_file, &serde_json::to_vec(&serving.ready)?)?;
    if let Some(trigger) = args.register_when {
        let engine = Arc::clone(&serving.engine);
        let uri = serving.ready.uri.clone();
        tokio::spawn(async move {
            while !trigger.exists() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let outcome = RegisterWhenDone {
                error: engine
                    .register_deployment(&uri)
                    .await
                    .err()
                    .map(|error| error.to_string()),
            };
            let mut done = trigger.into_os_string();
            done.push(".done");
            if let Ok(bytes) = serde_json::to_vec(&outcome) {
                let _ = write_atomically(Path::new(&done), &bytes);
            }
        });
    }
    shutdown_signal().await?;
    serving.stop().await;
    Ok(())
}

/// What a `serve --register-when` node's later registration answered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterWhenDone {
    /// The registration's error, when it was refused or failed.
    pub error: Option<String>,
}

/// SIGTERM or SIGINT, whichever comes first.
async fn shutdown_signal() -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install the SIGTERM handler")?;
    tokio::select! {
        _ = terminate.recv() => {}
        interrupted = tokio::signal::ctrl_c() => interrupted.context("wait for SIGINT")?,
    }
    Ok(())
}

/// Write `bytes` beside `path` and rename it into place, so a poller never
/// reads a half-written file.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let staging = path.with_extension("partial");
    std::fs::write(&staging, bytes).with_context(|| format!("write {}", staging.display()))?;
    std::fs::rename(&staging, path)
        .with_context(|| format!("rename {} to {}", staging.display(), path.display()))
}

async fn turn(args: TurnArgs) -> Result<TurnReport> {
    let stores = open_stores(&args.store).await?;
    let engine = engine(stores, &args.restate)?;
    let core = core(lash::Backend::new(engine), &ProviderArgs::default())?;
    let session_id = lash::SessionId::from(args.session.clone());
    let session = core
        .session(session_id)
        .create()
        .await
        .map_err(|error| anyhow!("create session {}: {error}", args.session))?;
    let settle = async {
        let handle = session
            .send(lash::TurnInput::text(args.message.clone()))
            .into_future()
            .await
            .map_err(|error| anyhow!("send to {}: {error}", args.session))?;
        handle
            .outcome()
            .await
            .map_err(|error| anyhow!("await the turn on {}: {error}", args.session))
    };
    let outcome = tokio::time::timeout(Duration::from_secs(args.timeout_secs), settle)
        .await
        .map_err(|_| {
            anyhow!(
                "the turn on {} did not settle within {}s",
                args.session,
                args.timeout_secs
            )
        })??;
    let status = format!("{:?}", outcome.status);
    let reply = outcome
        .output
        .as_ref()
        .and_then(|output| output.assistant_message())
        .map(str::to_string);
    if !matches!(outcome.status, lash::TurnStatus::Answered) {
        bail!(
            "the turn on {} settled {status}, not Answered (reply {reply:?})",
            args.session
        );
    }
    Ok(TurnReport {
        host: BuildLabel::current(),
        session: args.session,
        status,
        reply,
    })
}

async fn register(args: RegisterArgs) -> Result<RegisterReport> {
    let stores = open_stores(&args.store).await?;
    let engine = engine(stores, &args.restate)?;
    let registered = match engine.register_deployment(&args.uri).await {
        Ok(()) => Ok(()),
        Err(lash_restate::RestateRegistrationError::EndpointServesAnotherGeneration {
            uri,
            held,
            local,
        }) => Err(EndpointServesAnotherGeneration {
            uri,
            held: held.map(|held| held.to_string()),
            local: local.to_string(),
        }),
        Err(error) => bail!("register {}: {error}", args.uri),
    };
    Ok(RegisterReport {
        build: BuildLabel::current(),
        uri: args.uri,
        registered,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{StoreSpec, served_by};
    use crate::identity::BuildLabel;

    #[test]
    fn store_specs_name_their_backend() {
        assert_eq!(
            "postgres://lash:lash@127.0.0.1:5432/lash".parse::<StoreSpec>(),
            Ok(StoreSpec::Postgres(
                "postgres://lash:lash@127.0.0.1:5432/lash".to_string()
            ))
        );
        assert_eq!(
            "sqlite:/tmp/stores".parse::<StoreSpec>(),
            Ok(StoreSpec::Sqlite(PathBuf::from("/tmp/stores")))
        );
        assert!("/tmp/stores".parse::<StoreSpec>().is_err());
        let sqlite = StoreSpec::Sqlite(PathBuf::from("/tmp/stores"));
        assert_eq!(sqlite.to_string().parse::<StoreSpec>(), Ok(sqlite));
    }

    #[test]
    fn the_reply_names_the_serving_build() {
        assert_eq!(
            served_by(BuildLabel::Next, "0123456789ab"),
            "served by n+1 at generation 0123456789ab"
        );
    }
}
