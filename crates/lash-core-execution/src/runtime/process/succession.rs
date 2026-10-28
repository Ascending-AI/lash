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

use super::engine::ProcessEngineRegistry;

use lash_core_store::process_identity::ProcessEnvRef;
use lash_execution_contracts::process::ProcessInput;

use crate::ProcessEnvRefusal;
use lash_core_store::artifact_referrer::ArtifactReferrer;
use lash_core_store::artifact_referrer::ReferrerClaim;
use lash_execution_contracts::backend::Backend;
use lash_execution_contracts::plugin_error::PluginError;
use lash_vocabulary::identity::ProcessId;

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
    if let Some(id) = &successor.request_id {
        let request = ProcessSuccessorRequest {
            unchanged: false,
            request_id: Some(id.clone()),
            payload: successor.payload.clone(),
            env: successor.env.clone(),
            definition_id: None,
        };
        for prefix in ["process.succeeded", "process.successor_refused"] {
            let prior = backend
                .process_registry()
                .event_by_replay_key(
                    process,
                    &request.replay_key(process, prefix),
                    record.last_event_sequence,
                )
                .await?;
            if !matches!(prior, ProcessEventReadOutcome::Retained(None)) {
                return Ok(ProcessSuccession::AlreadyResolved);
            }
        }
    }
    if record.is_terminal() {
        return Err(refused(ProcessSuccessorRefusal::Ended));
    }
    let ProcessInput::Engine { kind, payload, .. } = &*record.input else {
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
        unchanged: false,
        request_id: successor.request_id,
        payload: successor.payload,
        env: successor.env,
        definition_id,
    };
    if request.request_id.is_none()
        && lash_execution_contracts::process::succession::successor_is_current(&request, &record)
    {
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
    let inspection = engines
        .inspect_code(kind, &request.payload)
        .await
        .map_err(PluginError::from)?;
    // The stored run carries onto the new payload, or the request is
    // refused now and the process goes on as written.
    let state = crate::runtime::actor::process::stored_engine_state(
        backend.durable().as_ref(),
        process,
        backend.config().settings().graph_reads.engine(),
    )
    .await
    .map_err(|error| PluginError::Session(format!("reading the process's state: {error}")))?;
    let context = succession_context(
        backend,
        engines,
        process,
        kind,
        payload,
        &request.payload,
        &new_env,
    )
    .await?;
    match succession.carry(process, &context, state.as_ref()).await {
        Err(error) => return Err(refused(carry_refusal(error)?)),
        Ok(SuccessorCarry::Carried {
            correspondence,
            predecessor_code,
            resolutions,
            ..
        }) => {
            let (merged, inspection) = crate::runtime::actor::process::merge_successor_debt(
                engines.require(kind)?.as_ref(),
                backend.package_store().as_ref(),
                &context,
                &new_env,
                &inspection,
                &predecessor_code,
            )
            .await
            .map_err(PluginError::from)?;
            let mut merged_context = context.clone();
            merged_context.environment = merged.execution().clone();
            crate::runtime::actor::process::validate_successor_pins(&merged_context, &inspection, &correspondence, &resolutions)
                .map_err(|message| refused(ProcessSuccessorRefusal::Engine {
                    refusal: serde_json::json!({"kind":"incompatible_succession", "message":message}), message,
                }))?;
            let seeds: Vec<_> = resolutions
                .into_iter()
                .map(|record| lash_core_store::resolutions::ResolutionSeed {
                    use_: record.key.use_,
                    entry: record.entry,
                })
                .collect();
            engines
                .check_links_with_seeds(&std::sync::Arc::new(merged), &inspection, &seeds)
                .map_err(PluginError::from)?;
        }
        Ok(SuccessorCarry::InFlight) => {}
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
        super::HoldEnvError::Admission(error) => error.into(),
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

pub async fn succession_context(
    backend: &Backend,
    engines: &ProcessEngineRegistry,
    process: &ProcessId,
    kind: &str,
    from: &serde_json::Value,
    to: &serde_json::Value,
    environment: &crate::ProcessEnv,
) -> Result<lash_execution_contracts::process::SuccessionContext, PluginError> {
    let record = backend
        .process_registry()
        .get_process(process)
        .await?
        .ok_or_else(|| PluginError::Session("successor predecessor is missing".into()))?;
    let ProcessInput::Engine { launch, .. } = record.input.as_ref() else {
        return Err(PluginError::Session(
            "successor predecessor is not an engine".into(),
        ));
    };
    let predecessor = lash_execution_contracts::process::load_process_env(
        backend.process_env_store().as_ref(),
        &launch.env,
    )
    .await
    .map_err(|error| PluginError::Session(error.to_string()))?;
    let execution = lash_core_store::resolutions::ExecutionId::process(process);
    let from_payload = from.clone();
    let to_payload = to.clone();
    let from = engines
        .inspect_code(kind, &from_payload)
        .await
        .map_err(PluginError::from)?;
    let to = engines
        .inspect_code(kind, &to_payload)
        .await
        .map_err(PluginError::from)?;
    let store = backend.stores().execution_resolutions();
    let mut resolutions = std::collections::BTreeMap::new();
    for use_ in from
        .operations
        .keys()
        .chain(predecessor.execution().uses.keys())
    {
        if let Some(record) = store
            .load(&lash_core_store::resolutions::ResolutionKey {
                execution: execution.clone(),
                use_: *use_,
            })
            .await
            .map_err(lash_execution_contracts::plugin_error::registry_error)?
        {
            resolutions.insert(*use_, record.entry);
        }
    }
    Ok(lash_execution_contracts::process::SuccessionContext {
        execution,
        from,
        to,
        from_payload,
        to_payload,
        environment: environment.execution().clone(),
        predecessor: predecessor.execution().clone(),
        resolutions,
    })
}
