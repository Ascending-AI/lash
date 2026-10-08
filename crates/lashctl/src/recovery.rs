//! Deployment recovery through the same core API embedders use.
use super::{CliError, Exit, USAGE, stalled_result};
use lash::{ObligationId, ObligationKind};
use serde_json::{Value, json};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(super) struct Invocation {
    pub(super) command: Command,
    sqlite_path: Option<PathBuf>,
}

pub(super) enum Command {
    Stalled {
        kind: ObligationKind,
        after: Option<ObligationId>,
        limit: NonZeroUsize,
    },
    Rearm {
        kind: ObligationKind,
        id: ObligationId,
    },
    DeploymentStatus {
        accepting_new_work: bool,
    },
}

impl Command {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Stalled { .. } => "stalled-list",
            Self::Rearm { .. } => "stalled-rearm",
            Self::DeploymentStatus { .. } => "deployment-status",
        }
    }
}

fn usage() -> CliError {
    CliError::new(Exit::Usage, USAGE)
}
fn kind(value: &str) -> Result<ObligationKind, CliError> {
    ObligationKind::ALL
        .into_iter()
        .find(|kind| kind.label() == value)
        .ok_or_else(usage)
}

/// Consume backend selection separately so every recovery verb
/// addresses the same store: the words left over, and the SQLite database
/// file `--sqlite-path` names.
pub(super) fn split_sqlite_path(rest: &[String]) -> Result<(Vec<&str>, Option<PathBuf>), CliError> {
    let mut words = Vec::new();
    let mut sqlite_path = None;
    let mut index = 0;
    while index < rest.len() {
        if rest[index] == "--sqlite-path" {
            if sqlite_path.is_some() || index + 1 == rest.len() || rest[index + 1].is_empty() {
                return Err(usage());
            }
            sqlite_path = Some(PathBuf::from(&rest[index + 1]));
            index += 2;
        } else {
            words.push(rest[index].as_str());
            index += 1;
        }
    }
    Ok((words, sqlite_path))
}

/// A cursor is opaque JSON; an obligation cursor is its plain id.
pub(super) fn parse(verb: &str, rest: &[String]) -> Result<Invocation, CliError> {
    let (words, sqlite_path) = split_sqlite_path(rest)?;
    let command = match (verb, words.as_slice()) {
        ("stalled", ["list", delivery, options @ ..]) => {
            let (after, limit) = page(options)?;
            Command::Stalled {
                kind: kind(delivery)?,
                after: after.map(ObligationId::new),
                limit,
            }
        }
        ("stalled", ["rearm", delivery, id]) if !id.is_empty() => Command::Rearm {
            kind: kind(delivery)?,
            id: ObligationId::new(*id),
        },
        ("deployment-status", ["--accepting-new-work", admission]) => Command::DeploymentStatus {
            accepting_new_work: admission.parse().map_err(|_| usage())?,
        },
        _ => return Err(usage()),
    };
    Ok(Invocation {
        command,
        sqlite_path,
    })
}

fn page<'a>(options: &[&'a str]) -> Result<(Option<&'a str>, NonZeroUsize), CliError> {
    let mut after = None;
    let mut limit = None;
    for pair in options.chunks(2) {
        let [flag, value] = pair else {
            return Err(usage());
        };
        match *flag {
            "--after" if after.is_none() => after = Some(*value),
            "--limit" if limit.is_none() => {
                limit = Some(value.parse::<NonZeroUsize>().map_err(|_| usage())?)
            }
            _ => return Err(usage()),
        }
    }
    let limit = limit.unwrap_or(NonZeroUsize::MIN.saturating_add(49));
    if limit.get() > 200 {
        return Err(usage());
    }
    Ok((after, limit))
}

/// The selected store: the SQLite database file `--sqlite-path` or
/// `LASH_SQLITE_PATH` names, else the PostgreSQL database.
pub(super) async fn open_stores(
    sqlite_path: Option<&Path>,
) -> Result<Arc<dyn lash::StoreSet>, CliError> {
    let sqlite_path = sqlite_path
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("LASH_SQLITE_PATH").map(PathBuf::from));
    Ok(if let Some(path) = sqlite_path {
        Arc::new(
            lash::sqlite::SqliteStoreSet::open(path)
                .await
                .map_err(|error| CliError::new(Exit::Unexpected, error.to_string()))?,
        )
    } else {
        let storage = lash::postgres::PostgresStorage::connect(
            &super::endpoints()?,
            &super::host_config()?,
            Default::default(),
        )
        .await
        .map_err(|error| CliError::new(Exit::Unexpected, error.to_string()))?;
        Arc::new(lash::postgres::PostgresStoreSet::new(
            &storage,
            lash::sqlite::SqliteStoreSet::open(std::path::Path::new(".lashctl-attachments.db"))
                .await
                .map_err(|error| CliError::new(Exit::Unexpected, error.to_string()))?
                .attachment_store(),
        ))
    })
}

/// The deployment's durable engine over `stores`.
pub(super) fn durable_backend(stores: Arc<dyn lash::StoreSet>) -> Result<lash::Backend, CliError> {
    lash::durable::DurableBackendBuilder::new(stores)
        .build()
        .map_err(|error| CliError::new(Exit::Unexpected, error.to_string()))
}

impl Invocation {
    async fn core(&self) -> Result<lash::LashCore, CliError> {
        let backend = durable_backend(open_stores(self.sqlite_path.as_deref()).await?)?;
        // This core sends control intents through the store. It serves no model,
        // starts no host turn, and does not install an HTTP handler endpoint.
        lash::LashCore::standard_builder(backend)
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
            .execution_budgets(lash::ExecutionBudgets::recommended())
            .delta_coalescing(lash::DeltaCoalescing::recommended())
            .build(lash_core_store::store::LeaseOwnerIdentity::opaque(
                "lashctl",
                std::process::id().to_string(),
            ))
            .map_err(core_error)
    }

    pub(super) async fn run(&self) -> Result<Value, CliError> {
        let core = self.core().await?;
        execute(&core, &self.command).await
    }
}

async fn execute(core: &lash::LashCore, command: &Command) -> Result<Value, CliError> {
    Ok(match command {
        Command::Stalled { kind, after, limit } => {
            let records = core
                .stalled_obligations(*kind, after.as_ref(), limit.saturating_add(1))
                .await
                .map_err(core_error)?;
            let next = (records.len() > limit.get()).then(|| records[limit.get() - 1].id.clone());
            json!({"records":records.iter().take(limit.get()).map(stalled_result).collect::<Vec<_>>(), "next":next})
        }
        Command::Rearm { kind, id } => {
            json!({"rearmed":core.rearm_obligation(*kind, id).await.map_err(core_error)?})
        }
        Command::DeploymentStatus { accepting_new_work } => json!(
            core.drain_status(*accepting_new_work)
                .await
                .map_err(core_error)?
        ),
    })
}

pub(super) fn core_error(error: lash::EmbedError) -> CliError {
    match error {
        lash::EmbedError::Store(error) => store_error(error),
        lash::EmbedError::Runtime(error) => {
            CliError::refused(Exit::Unexpected, error.to_string(), &error)
        }
        other => CliError::new(Exit::Unexpected, other.to_string()),
    }
}

fn store_error(error: lash_core_store::store::StoreError) -> CliError {
    use lash_core_store::store::StoreError;
    match error {
        error @ (StoreError::Incompatible { .. } | StoreError::WriterFenced { .. }) => {
            CliError::store(error)
        }
        error => {
            let exit = if error.is_transient() {
                Exit::Unexpected
            } else {
                Exit::Refused
            };
            let message = error.to_string();
            CliError::refused(
                exit,
                message,
                &lash::runtime::RuntimeEffectControllerError::from(error),
            )
        }
    }
}
