//! Deployment recovery through the same core API embedders use.
use super::{CliError, Exit, OPERATOR_POOL_MAX, USAGE, stalled_result};
use lash::{ObligationId, ObligationKind, ParkedWorkRef};
use serde_json::{Value, json};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(super) struct Invocation {
    pub(super) command: Command,
    sqlite_path: Option<PathBuf>,
}

pub(super) enum Command {
    Parks(lash::ParkedWorkQuery),
    Events {
        after: lash::ParkedWorkEventsCursor,
        limit: NonZeroUsize,
    },
    Park {
        verb: ParkVerb,
        target: ParkedWorkRef,
        park_id: lash::persistence::ParkId,
    },
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

pub(super) enum ParkVerb {
    Redrive,
    Cancel,
    Fork,
}

impl Command {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Parks(_) => "park-list",
            Self::Events { .. } => "park-events",
            Self::Park {
                verb: ParkVerb::Redrive,
                ..
            } => "park-redrive",
            Self::Park {
                verb: ParkVerb::Cancel,
                ..
            } => "park-cancel",
            Self::Park {
                verb: ParkVerb::Fork,
                ..
            } => "park-fork",
            Self::Stalled { .. } => "stalled-list",
            Self::Rearm { .. } => "stalled-rearm",
            Self::DeploymentStatus { .. } => "deployment-status",
        }
    }
}

fn usage() -> CliError {
    CliError::new(Exit::Usage, USAGE)
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, CliError> {
    serde_json::from_str(value).map_err(|error| CliError::new(Exit::Usage, error.to_string()))
}
fn kind(value: &str) -> Result<ObligationKind, CliError> {
    ObligationKind::ALL
        .into_iter()
        .find(|kind| kind.label() == value)
        .ok_or_else(usage)
}

/// Consume backend selection separately so every recovery and drain verb
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
        ("park", [action @ ("list" | "events"), options @ ..]) => {
            let (after, limit) = page(options)?;
            if *action == "list" {
                let mut query = lash::ParkedWorkQuery::all(limit);
                query.after = after.map(decode).transpose()?;
                Command::Parks(query)
            } else {
                Command::Events {
                    after: after.map(decode).transpose()?.unwrap_or_default(),
                    limit,
                }
            }
        }
        ("park", [action @ ("redrive" | "cancel" | "fork"), options @ ..]) => {
            let mut target = None;
            let mut park_id = None;
            for pair in options.chunks(2) {
                let [flag, value] = pair else {
                    return Err(usage());
                };
                match *flag {
                    "--target" if target.is_none() => target = Some(decode(value)?),
                    "--park-id" if park_id.is_none() => park_id = Some(decode(value)?),
                    _ => return Err(usage()),
                }
            }
            Command::Park {
                verb: match *action {
                    "redrive" => ParkVerb::Redrive,
                    "cancel" => ParkVerb::Cancel,
                    _ => ParkVerb::Fork,
                },
                target: target.ok_or_else(usage)?,
                park_id: park_id.ok_or_else(usage)?,
            }
        }
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
        let storage = lash::postgres::PostgresStorage::connect_with(
            &super::database_url()?,
            lash::postgres::PostgresStoreConfig {
                max_connections: OPERATOR_POOL_MAX,
                ..Default::default()
            },
        )
        .await
        .map_err(CliError::store)?;
        Arc::new(lash::postgres::PostgresStoreSet::new(
            &storage,
            Arc::new(lash::persistence::FileAttachmentStore::new(
                ".lashctl-attachments",
            )),
        ))
    })
}

/// The deployment's engine over `stores`: the Restate server
/// `RESTATE_INGRESS_URL` and `RESTATE_ADMIN_URL` name, for the authority
/// `RESTATE_AUTHORITY_ID` names, in `RESTATE_NAMESPACE`.
pub(super) fn restate_backend(stores: Arc<dyn lash::StoreSet>) -> Result<lash::Backend, CliError> {
    let authority = std::env::var("RESTATE_AUTHORITY_ID").map_err(|_| {
        CliError::new(
            Exit::Refused,
            "RESTATE_AUTHORITY_ID must name the deployment authority",
        )
    })?;
    let namespace = lash::restate::RestateNamespace::new(
        std::env::var("RESTATE_NAMESPACE").unwrap_or_default(),
    )
    .map_err(|error| CliError::new(Exit::Usage, error.to_string()))?;
    let config = lash::restate::RestateConfig::new(
        std::env::var("RESTATE_INGRESS_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into()),
        std::env::var("RESTATE_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:9070".into()),
        lash::restate::RestateAuthorityId::new(authority)
            .map_err(|error| CliError::new(Exit::Usage, error.to_string()))?,
    )
    .with_namespace(namespace);
    Ok(lash::Backend::new(Arc::new(
        lash::restate::RestateEngine::new(stores, config),
    )))
}

impl Invocation {
    async fn core(&self) -> Result<lash::LashCore, CliError> {
        let backend = restate_backend(open_stores(self.sqlite_path.as_deref()).await?)?;
        // This core sends control intents through Restate. It serves no model,
        // starts no host turn, and does not install an HTTP handler endpoint.
        lash::LashCore::standard_builder(backend)
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
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
        Command::Parks(query) => {
            let page = core.parked_work().list(query).await.map_err(core_error)?;
            json!({"records": page.records.into_iter().map(|park| json!({"target": park.target, "park_id": park.park_id, "reason": park.reason, "since_ms": park.since_ms, "last_refused_ms": park.last_refused_ms, "attempts": park.attempts})).collect::<Vec<_>>(), "next": page.next})
        }
        Command::Events { after, limit } => {
            let page = core
                .parked_work()
                .events(after, *limit)
                .await
                .map_err(core_error)?;
            json!({"events": page.events.into_iter().map(|event| json!({"target": event.target, "park_id": event.park_id, "at_ms": event.at_ms, "kind": event.kind})).collect::<Vec<_>>(), "next": page.next})
        }
        Command::Park {
            verb,
            target,
            park_id,
        } => match verb {
            ParkVerb::Redrive => match core
                .parked_work()
                .redrive(target, *park_id)
                .await
                .map_err(park_error)?
            {
                lash::RedriveAccepted::Run(result) => {
                    json!({"kind":"turn", "intent":result.intent, "applied":result.applied, "turn_id":result.run})
                }
                lash::RedriveAccepted::Process { process, park } => {
                    json!({"kind":"process", "process_id":process, "park_id":park})
                }
            },
            ParkVerb::Cancel => {
                let result = core
                    .parked_work()
                    .cancel(target, *park_id)
                    .await
                    .map_err(park_error)?;
                json!({"intent":result.intent, "terminal":result.terminal, "applied":result.applied})
            }
            ParkVerb::Fork => {
                let ParkedWorkRef::Turn {
                    session_id,
                    turn_id,
                } = target
                else {
                    return Err(park_error(lash::ParkVerbRefused::ForkRequiresTurn));
                };
                let result = core
                    .parked_work()
                    .fork(session_id, turn_id, *park_id)
                    .await
                    .map_err(park_error)?;
                json!({"intent":result.intent, "cancelled":result.cancelled, "new_turn":result.new_run, "applied":result.applied})
            }
        },
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

fn park_error(error: lash::ParkVerbRefused) -> CliError {
    use lash::ParkVerbRefused;
    let refusal = match &error {
        ParkVerbRefused::NotParked => json!({"kind":"not_parked"}),
        ParkVerbRefused::ParkSuperseded { current } => {
            json!({"kind":"park_superseded", "current":current})
        }
        ParkVerbRefused::Redriving { intent } => json!({"kind":"redriving", "intent":intent}),
        ParkVerbRefused::IntentOpen { intent } => json!({"kind":"intent_open", "intent":intent}),
        ParkVerbRefused::SessionDeleted => json!({"kind":"session_deleted"}),
        ParkVerbRefused::SessionClosing => json!({"kind":"session_closing"}),
        ParkVerbRefused::ForkRequiresTurn => json!({"kind":"fork_requires_turn"}),
        ParkVerbRefused::SubstrateRefused { code, message } => {
            json!({"kind":"engine_refused", "code":code, "message":message})
        }
        ParkVerbRefused::Store(_) => {
            return match error {
                ParkVerbRefused::Store(error) => store_error(error),
                _ => unreachable!(),
            };
        }
        _ => json!({"kind":"park_refused", "message":error.to_string()}),
    };
    CliError::refused(Exit::Refused, error.to_string(), &refusal)
}

fn store_error(error: lash_core_store::store::StoreError) -> CliError {
    use lash_core_store::store::StoreError;
    match error {
        error @ (StoreError::Incompatible { .. } | StoreError::WriterFenced { .. }) => {
            CliError::store(error)
        }
        StoreError::ParkFeedCursorCompacted { horizon } => CliError::refused(
            Exit::Refused,
            "park event cursor was compacted; relist and restart the feed".into(),
            &json!({"kind":"cursor_compacted", "horizon":horizon}),
        ),
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
