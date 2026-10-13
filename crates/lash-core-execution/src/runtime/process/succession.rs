//! A host carries a process onto a successor pair (FIG-6121, D2).
//!
//! A process is the immutable pair of its engine payload and its launch env,
//! and it stays pinned to what that pair names. Moving it onto another pair is
//! the host's explicit operation, never lash's own: the host names the new
//! pair, lash link-checks and holds it under the process's referrer, and
//! records the request as `process.successor` mail to the process's actor
//! ([`request_process_successor`]).
//!
//! The process's owner applies it under its claim fence. The pass that first
//! sees the mail records the request in the process's driver, and the first
//! pass that finds the run at rest applies it. At rest means nothing of the
//! activation runs, and the engine's [`EngineSuccession`] answers the run
//! parked. The apply commit carries the engine state onto the new payload
//! and appends `process.succeeded`, whose fold rewrites the record's input
//! and definition id. It writes both or neither, so a crash leaves the old
//! pair with the request pending, or the new pair whole. A request the
//! engine refuses at the apply records `process.successor_refused`, and the
//! process goes on as written.
//!
//! An admitted step keeps its call and the match its admission recorded: a
//! request whose env links an in-flight step's effect to anything else is
//! refused.

use std::collections::BTreeMap;

use lash_kernel_doc::EffectName;
use serde::{Deserialize, Serialize};

use super::engine::ProcessEngineRegistry;
use super::engine_state::EngineState;
use super::model::{ProcessEnvRef, ProcessInput, ProcessRecord};
use super::{FitPolicy, ProcessDefinitionId};
use crate::{ArtifactReferrer, Backend, PluginError, ProcessEnvRefusal, ProcessId, ReferrerClaim};

/// The mail kind that carries a [`ProcessSuccessorRequest`] to the process's
/// actor.
pub const SUCCESSOR_MAIL: &str = "process.successor";

/// The pair a host names for a process to go on as: the engine start
/// payload and the published env it runs under, with the fit each effect's
/// link is checked at (the default is [`FitPolicy::Compatible`]).
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessSuccessor {
    pub payload: serde_json::Value,
    pub env: ProcessEnvRef,
    pub fits: BTreeMap<EffectName, FitPolicy>,
}

impl ProcessSuccessor {
    #[must_use]
    pub fn new(payload: serde_json::Value, env: ProcessEnvRef) -> Self {
        Self {
            payload,
            env,
            fits: BTreeMap::new(),
        }
    }
}

/// A successor request as it is recorded: the mail's body, and the
/// driver's pending request until its owner applies it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessSuccessorRequest {
    pub payload: serde_json::Value,
    pub env: ProcessEnvRef,
    /// The definition the engine's admission names the new payload by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition_id: Option<ProcessDefinitionId>,
}

impl ProcessSuccessorRequest {
    /// Whether `record` already runs this pair.
    #[must_use]
    pub(crate) fn is_current(&self, record: &ProcessRecord) -> bool {
        match record.input.as_ref() {
            ProcessInput::Engine {
                payload, launch, ..
            } => *payload == self.payload && launch.env == self.env,
            ProcessInput::SessionTurn { .. } => false,
        }
    }

    /// The replay key of the event `prefix` records this request as.
    #[must_use]
    pub(crate) fn replay_key(&self, process: &ProcessId, prefix: &str) -> String {
        let digest = crate::stable_hash::sha256_hex(&crate::identity_json::payload_leaf(
            &serde_json::to_value(self).unwrap_or_default(),
        ));
        format!("{prefix}:{process}:{digest}")
    }
}

/// What a successor request answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessSuccession {
    /// It was recorded: the process's owner applies it once the run is at
    /// rest, or records why it refused.
    Requested,
    /// The process already runs the named pair; nothing was recorded.
    AlreadyCurrent,
}

/// Why a successor request is refused. A refusal records nothing, and the
/// process goes on as written.
#[derive(
    Clone, Debug, PartialEq, Serialize, Deserialize, schemars::JsonSchema, thiserror::Error,
)]
#[serde(tag = "refused", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum ProcessSuccessorRefusal {
    #[error("no such process")]
    UnknownProcess,
    #[error("the process has ended")]
    Ended,
    #[error("the process runs no engine")]
    NotEngine,
    #[error("process engine `{kind}` carries no process onto a successor")]
    Unsupported { kind: String },
    /// The new pair does not link (P1): items 17 and 21.
    #[error("the successor pair does not link: {refusal}")]
    Link { refusal: ProcessEnvRefusal },
    /// The new env links `effect`, which an admitted step of the process
    /// performs, otherwise than the step's admission matched it.
    #[error("an admitted step performs `{effect}`, which the successor env links otherwise")]
    AdmittedStepRelinked { effect: String },
    #[error("the process is being cancelled")]
    Cancelling,
    /// The engine's own reason, as its data: for a kernel process, why no
    /// correspondence carries its run onto the new document.
    #[error("{message}")]
    Engine {
        refusal: serde_json::Value,
        message: String,
    },
}

/// What an engine's [`EngineSuccession::carry`] answers.
#[derive(Clone, Debug, PartialEq)]
pub enum SuccessorCarry {
    /// The state carried onto the successor payload; `None` for a process
    /// whose engine has not started.
    Carried {
        state: Option<EngineState>,
        /// The carried state pins what this build's engine writes, such as
        /// its own helper release: the process's actor is stamped with the
        /// engine's own format set, so only a node that holds them claims
        /// it from now on.
        own_formats: bool,
    },
    /// The run is in flight: it is carried at its next park.
    InFlight,
}

/// Why an engine did not carry a state onto a successor.
#[derive(Clone, Debug, PartialEq)]
pub enum SuccessionError {
    /// The engine refuses, typed: no correspondence carries the run.
    Refused {
        refusal: serde_json::Value,
        message: String,
    },
    /// A store did not answer; asked again later.
    Unavailable(String),
}

/// How an engine carries a process onto a successor payload its host
/// names (D2): the engine's own correspondence carry. Pure over what it
/// reads; it writes nothing, and the process's owner commits its answer.
#[async_trait::async_trait]
pub trait EngineSuccession: Send + Sync {
    /// `state`, a run of `from` (`None` before the engine's first
    /// transition), carried onto `to`.
    async fn carry(
        &self,
        process: &ProcessId,
        from: &serde_json::Value,
        to: &serde_json::Value,
        state: Option<&EngineState>,
    ) -> Result<SuccessorCarry, SuccessionError>;
}

/// The refusal of a request whose new env links an effect of an admitted,
/// unsettled step otherwise than `old` does.
pub(crate) fn relinked_step(
    in_flight: impl IntoIterator<Item = EffectName>,
    old: &crate::ProcessEnv,
    new: &crate::ProcessEnv,
) -> Option<ProcessSuccessorRefusal> {
    in_flight
        .into_iter()
        .find(|effect| old.effects().get(effect) != new.effects().get(effect))
        .map(|effect| ProcessSuccessorRefusal::AdmittedStepRelinked {
            effect: effect.to_string(),
        })
}

/// The refusal an engine's carry error is answered with, or the fault of
/// one that did not answer.
pub(crate) fn carry_refusal(
    error: SuccessionError,
) -> Result<ProcessSuccessorRefusal, PluginError> {
    match error {
        SuccessionError::Refused { refusal, message } => {
            Ok(ProcessSuccessorRefusal::Engine { refusal, message })
        }
        SuccessionError::Unavailable(message) => Err(PluginError::StoreUnavailable {
            fault: crate::store::StoreFault::Backend { message },
        }),
    }
}

/// Ask that `process` go on as `successor`. Checks and holds the new pair,
/// asks its engine whether the stored run carries onto it, and records the
/// request for the process's owner, who applies it at the run's next rest.
///
/// # Errors
///
/// [`PluginError::ProcessSuccessorRefused`] with the typed reason, which
/// records nothing; any other error is a fault a retry may not meet.
pub async fn request_process_successor(
    backend: &Backend,
    engines: &ProcessEngineRegistry,
    process: &ProcessId,
    successor: ProcessSuccessor,
) -> Result<ProcessSuccession, PluginError> {
    let refused = |refusal| PluginError::ProcessSuccessorRefused {
        process: process.clone(),
        refusal: Box::new(refusal),
    };
    let record = backend
        .process_registry()
        .get_process(process)
        .await?
        .ok_or_else(|| refused(ProcessSuccessorRefusal::UnknownProcess))?;
    if record.is_terminal() {
        return Err(refused(ProcessSuccessorRefusal::Ended));
    }
    let ProcessInput::Engine {
        kind,
        payload,
        launch,
    } = &*record.input
    else {
        return Err(refused(ProcessSuccessorRefusal::NotEngine));
    };
    let succession = engines
        .succession(kind)
        .ok_or_else(|| refused(ProcessSuccessorRefusal::Unsupported { kind: kind.clone() }))?;
    let definition_id = engines
        .admit(kind, &successor.payload)
        .await?
        .into_identity()
        .definition_id;
    let request = ProcessSuccessorRequest {
        payload: successor.payload,
        env: successor.env,
        definition_id,
    };
    if request.is_current(&record) {
        return Ok(ProcessSuccession::AlreadyCurrent);
    }
    if record.cancel_request.is_some() {
        return Err(refused(ProcessSuccessorRefusal::Cancelling));
    }
    let link = |refusal| refused(ProcessSuccessorRefusal::Link { refusal });
    let env_store = backend.process_env_store();
    let load = |env_ref: ProcessEnvRef| {
        let env_store = std::sync::Arc::clone(&env_store);
        async move {
            super::model::load_process_env(env_store.as_ref(), &env_ref)
                .await
                .map_err(|error| match error {
                    super::ProcessEnvLoadError::Missing(env_ref) => {
                        link(ProcessEnvRefusal::Unpublished {
                            env_ref: env_ref.to_string(),
                        })
                    }
                    other => PluginError::Session(other.to_string()),
                })
        }
    };
    let new_env = load(request.env.clone()).await?;
    let performed = engines
        .performed_effects(kind, &request.payload)
        .await
        .map_err(link)?;
    super::check_links(&new_env, &performed, &successor.fits).map_err(link)?;
    engines
        .components()
        .check_served(&std::sync::Arc::new(new_env.clone()))
        .map_err(link)?;
    // An admitted step keeps the match its admission recorded.
    let row =
        backend.durable().process(process).await.map_err(|error| {
            PluginError::Session(format!("reading the process's driver: {error}"))
        })?;
    let in_flight = crate::runtime::actor::process::in_flight_effects(
        row.as_ref().and_then(|row| row.driver_json.as_deref()),
    )
    .map_err(|error| PluginError::Session(format!("the process's driver: {error}")))?;
    let old_env = load(launch.env.clone()).await?;
    if let Some(refusal) = relinked_step(in_flight, &old_env, &new_env) {
        return Err(refused(refusal));
    }
    // The stored run carries onto the new payload, or the request is
    // refused now and the process goes on as written.
    let state = crate::runtime::actor::process::stored_engine_state(
        backend.durable().as_ref(),
        process,
        backend.config().settings().graph_reads.engine(),
    )
    .await
    .map_err(|error| PluginError::Session(format!("reading the process's state: {error}")))?;
    if let Err(error) = succession
        .carry(process, payload, &request.payload, state.as_ref())
        .await
    {
        return Err(refused(carry_refusal(error)?));
    }
    hold(backend, engines, process, kind, &request).await?;
    let body = serde_json::to_string(&request)
        .map_err(|error| PluginError::Session(format!("encoding a successor request: {error}")))?;
    let actor = lash_durable::ActorKey::process(process.as_str())
        .map_err(|error| PluginError::Session(error.to_string()))?;
    let mut tx = lash_durable::MailTx::new();
    tx.append(actor, lash_durable::MailKind::new(SUCCESSOR_MAIL), body);
    match backend
        .commit_mail(tx, lash_durable::CommitLabel::MAIL_PROCESS)
        .await
    {
        Ok(_) => Ok(ProcessSuccession::Requested),
        Err(lash_durable::DurableError::MailRefused(
            lash_durable::MailRefusal::ActorTerminal(_)
            | lash_durable::MailRefusal::UnknownActor(_),
        )) => Err(refused(ProcessSuccessorRefusal::Ended)),
        Err(error) => Err(crate::runtime::actor::process::registry_error(error)),
    }
}

/// Hold `request`'s env and the artifacts its payload names under
/// `process`'s own referrer, as a start holds its own.
async fn hold(
    backend: &Backend,
    engines: &ProcessEngineRegistry,
    process: &ProcessId,
    kind: &str,
    request: &ProcessSuccessorRequest,
) -> Result<(), PluginError> {
    let claim = ReferrerClaim::unguarded(ArtifactReferrer::ProcessRecord(process.clone()))
        .map_err(|error| PluginError::Session(error.to_string()))?;
    let env_store = backend.process_env_store();
    super::hold_process_env(
        &super::ProcessEnvPorts::published_only(env_store.as_ref()),
        &claim,
        &super::ProcessEnvSelection::Published(request.env.clone()),
    )
    .await
    .map_err(|error| match error {
        super::HoldEnvError::Refused(refusal) => PluginError::ProcessSuccessorRefused {
            process: process.clone(),
            refusal: Box::new(ProcessSuccessorRefusal::Link { refusal }),
        },
        super::HoldEnvError::Fault(error) => error,
    })?;
    let names = engines.require(kind)?.start_artifacts(&request.payload)?;
    if names.is_empty() {
        return Ok(());
    }
    let ports = engines.artifact_ports().ok_or_else(|| {
        PluginError::Session(format!(
            "a successor of a `{kind}` process names artifacts, and the engine registry has no artifact stores to hold them"
        ))
    })?;
    ports.acquire(engines, &claim, &names).await?;
    Ok(())
}
