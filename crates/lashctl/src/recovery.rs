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
    /// The kernel processes and sessions still in the previous kernel
    /// version, each one this build's kernel migration would refuse with its
    /// typed reason (a session's by the cell its open turn stopped in), and
    /// each retired library function with the processes that depend on it.
    /// It also lists the processes and sessions that still pin an earlier
    /// helper release. Each such process stays on the helpers it pins until
    /// it ends or its host carries it onto a successor pair
    /// (`Processes::succeed`); the listing gives the typed reason its run
    /// would not carry onto this build's helpers. Run with the next build's
    /// `lashctl` before the upgrade; it writes nothing.
    KernelMigrationList,
    /// Wake every listed process and session, so a live node of the next
    /// build carries it forward: what an operator runs before rolling out
    /// the build that closes the window. Idempotent; run again to resume.
    KernelMigrationRun,
    /// The strand report (FIG-6993): each unfinished actor no live node
    /// can run, with what the closest live node lacks of its hard
    /// requirements (natives, kernels, providers, codecs). Such work stays
    /// parked; a deploy that offers what it lacks runs it. It writes
    /// nothing.
    Stranded {
        after: Option<String>,
        limit: NonZeroUsize,
    },
}

impl Command {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Stalled { .. } => "stalled-list",
            Self::Rearm { .. } => "stalled-rearm",
            Self::DeploymentStatus { .. } => "deployment-status",
            Self::KernelMigrationList => "kernel-migration-list",
            Self::KernelMigrationRun => "kernel-migration-run",
            Self::Stranded { .. } => "stranded-list",
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
        ("kernel-migration", ["list"]) => Command::KernelMigrationList,
        ("kernel-migration", ["run"]) => Command::KernelMigrationRun,
        ("stranded", ["list", options @ ..]) => {
            let (after, limit) = page(options)?;
            Command::Stranded {
                after: after.map(str::to_owned),
                limit,
            }
        }
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
            lash::sqlite::SqliteStoreSet::open(path, lash::sqlite::SqliteSynchronous::Normal)
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
            lash::sqlite::SqliteStoreSet::open(
                std::path::Path::new(".lashctl-attachments.db"),
                lash::sqlite::SqliteSynchronous::Normal,
            )
            .await
            .map_err(|error| CliError::new(Exit::Unexpected, error.to_string()))?
            .attachment_store(),
        ))
    })
}

/// The deployment's durable engine over `stores`.
pub(super) fn durable_backend(stores: Arc<dyn lash::StoreSet>) -> Result<lash::Backend, CliError> {
    lash::durable::DurableBackendBuilder::new(stores)
        .config(lash::durable::DurableSettings::standard())
        .build()
        .map_err(|error| CliError::new(Exit::Unexpected, error.to_string()))
}

impl Invocation {
    async fn core(&self) -> Result<lash::LashCore, CliError> {
        let backend = durable_backend(open_stores(self.sqlite_path.as_deref()).await?)?;
        // This core sends control intents through the store. It serves no model,
        // starts no host turn, and does not install an HTTP handler endpoint.
        lash::LashCore::standard_builder(backend)
            .data_retention(lash::DataRetention::standard())
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .execution_budgets(lash::ExecutionBudgets::recommended())
            .delta_coalescing(lash::DeltaCoalescing::recommended())
            .build(lash_core_store::store::LeaseOwnerIdentity::opaque(
                lash_core_store::store::LeaseOwnerId::new("lashctl"),
                lash_core_store::store::LeaseIncarnationId::new(std::process::id().to_string()),
            ))
            .map_err(core_error)
    }

    pub(super) async fn run(&self) -> Result<Value, CliError> {
        if matches!(
            self.command,
            Command::KernelMigrationList | Command::KernelMigrationRun
        ) {
            let backend = durable_backend(open_stores(self.sqlite_path.as_deref()).await?)?;
            return kernel_migration(&backend, &self.command).await;
        }
        if let Command::Stranded { after, limit } = &self.command {
            let backend = durable_backend(open_stores(self.sqlite_path.as_deref()).await?)?;
            return stranded(&backend, after.as_deref(), *limit).await;
        }
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
        // Answered from the backend, before a core is built.
        Command::KernelMigrationList | Command::KernelMigrationRun | Command::Stranded { .. } => {
            return Err(usage());
        }
    })
}

/// The survey of `backend`'s kernel processes and sessions against the
/// migration this build ships, over the library its shipped worker holds;
/// for `run`, the sweep that wakes each one still in the previous version,
/// and the survey after it.
async fn kernel_migration(backend: &lash::Backend, command: &Command) -> Result<Value, CliError> {
    let unexpected =
        |error: &dyn std::fmt::Display| CliError::new(Exit::Unexpected, error.to_string());
    let swept = match command {
        Command::KernelMigrationRun => Some(
            lash::vm::sweep_kernel_migration(backend)
                .await
                .map_err(|error| unexpected(&error))?,
        ),
        _ => None,
    };
    let functions = lash::vm::helper_release()
        .map(|release| std::sync::Arc::new(release.functions.clone()))
        .map_err(|error| unexpected(&error))?;
    let survey = lash::vm::survey_kernel_migration(backend, &functions)
        .await
        .map_err(|error| unexpected(&error))?;
    match swept {
        Some(swept) => Ok(json!({"woken": swept, "survey": survey})),
        None => serde_json::to_value(&survey).map_err(|error| unexpected(&error)),
    }
}

/// One page of `backend`'s strand report, after the actor key `after`.
async fn stranded(
    backend: &lash::Backend,
    after: Option<&str>,
    limit: NonZeroUsize,
) -> Result<Value, CliError> {
    let after = after
        .map(lash::durable::ActorKey::parse)
        .transpose()
        .map_err(|_| usage())?;
    let page = lash::durable::strand_report(backend, after.as_ref(), limit.get())
        .await
        .map_err(|error| CliError::new(Exit::Unexpected, error.to_string()))?;
    serde_json::to_value(&page).map_err(|error| CliError::new(Exit::Unexpected, error.to_string()))
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
