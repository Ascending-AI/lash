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

use lash_execution_contracts::process::*;
use lash_kernel_doc::EffectName;

use super::engine::ProcessEngineRegistry;

use lash_core_store::process_identity::ProcessEnvRef;
use lash_execution_contracts::process::ProcessInput;

use crate::ProcessEnvRefusal;
use lash_core_store::artifact_referrer::ArtifactReferrer;
use lash_core_store::artifact_referrer::ReferrerClaim;
use lash_execution_contracts::backend::Backend;
use lash_execution_contracts::plugin_error::PluginError;
use lash_protocol_types::ProcessId;

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
            fault: lash_core_store::store::StoreFault::Backend { message },
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
    if lash_execution_contracts::process::succession::successor_is_current(&request, &record) {
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
            lash_execution_contracts::process::load_process_env(env_store.as_ref(), &env_ref)
                .await
                .map_err(|error| match error {
                    lash_execution_contracts::process::ProcessEnvLoadError::Missing(env_ref) => {
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
        Err(error) => Err(lash_execution_contracts::plugin_error::registry_error(
            error,
        )),
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
