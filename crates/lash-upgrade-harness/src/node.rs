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
//!
//! The scripted provider answers every model call with the serving build's
//! label and `G`, so a turn's reply names the build that drove it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};

use crate::identity::BuildLabel;

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
    /// Serve a Restate deployment over a store until killed.
    Serve(ServeArgs),
    /// Send one input to a session and wait for its turn to settle.
    Turn(TurnArgs),
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

/// The reply the scripted provider gives on `build` at `generation`.
pub fn served_by(build: BuildLabel, generation: &str) -> String {
    format!("served by {build} at generation {generation}")
}

/// Run one command and print its report.
pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Serve(args) => serve(args).await,
        Command::Turn(args) => print(&turn(args).await?),
    }
}

fn print(report: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string(report)?);
    Ok(())
}

async fn open_sqlite(dir: &Path) -> Result<lash::sqlite::SqliteStoreSet> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    lash::sqlite::SqliteStoreSet::open(dir)
        .await
        .map_err(|error| anyhow!("open the SQLite store set at {}: {error}", dir.display()))
}

async fn open_stores(args: &StoreArgs) -> Result<Arc<dyn lash::StoreSet>> {
    match &args.store {
        StoreSpec::Postgres(url) => {
            let storage = lash_postgres_store::PostgresStorage::connect(url)
                .await
                .map_err(|error| anyhow!("open the PostgreSQL store: {error}"))?;
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

/// The core every node builds: the scripted provider over `backend`. A host
/// never calls the provider; the node that drives a turn does.
fn core(backend: lash::Backend) -> Result<lash::LashCore> {
    let build = BuildLabel::current();
    let reply = served_by(build, &lash::formats::build_generation().to_string());
    let provider = lash_core::testing::TestProvider::builder()
        .kind("upgrade-harness")
        .options(lash_core::provider::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::disabled(),
            ..lash_core::provider::ProviderOptions::default()
        })
        // Both builds serialize the same config, so a session one build
        // opened reads as the same provider on the other.
        .serialize_config(|| serde_json::json!({ "fixture": "upgrade-harness" }))
        .complete(move |_request| {
            let reply = reply.clone();
            async move { Ok(scripted_reply(reply)) }
        })
        .build();
    lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .provider(provider.into_handle())
        .model(
            lash::ModelSpec::builder("upgrade-harness-model")
                .context_window_tokens(200_000)
                .build()
                .map_err(|error| anyhow!("model spec: {error}"))?,
        )
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

async fn serve(args: ServeArgs) -> Result<()> {
    let stores = open_stores(&args.store).await?;
    let engine = engine(stores, &args.restate)?;
    let core = core(lash::Backend::new(engine.clone()))?;
    let worker = lash::durability::DurableProcessWorker::new(
        core.durable_process_worker_config()
            .context("process worker config")?,
    )
    .map_err(|error| anyhow!("build the process worker: {error}"))?;
    let endpoint = engine.endpoint_builder(worker).build();
    let listener = tokio::net::TcpListener::bind(args.bind)
        .await
        .with_context(|| format!("bind the endpoint at {}", args.bind))?;
    let uri = format!("http://{}", listener.local_addr()?);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(async move {
        lash::restate::serve_endpoint(listener, endpoint, async move {
            let _ = stopped.await;
        })
        .await;
    });
    engine
        .register_deployment(&uri)
        .await
        .with_context(|| format!("register the deployment at {uri}"))?;
    let ready = ServeReady {
        build: BuildLabel::current(),
        generation: engine.build_generation().to_string(),
        uri,
    };
    write_atomically(&args.ready_file, &serde_json::to_vec(&ready)?)?;
    shutdown_signal().await?;
    let _ = stop.send(());
    let _ = serving.await;
    // Keep the core alive for as long as the endpoint serves.
    drop(core);
    Ok(())
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
    let core = core(lash::Backend::new(engine))?;
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
