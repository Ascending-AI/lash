//! The store half of one process start: stage what the start carries under
//! the start's referrer, then register the row (ADR 0113 §3.3).
//!
//! Staging is an acquisition of `Start(key)`, guarded by `AwaitStart` on the
//! starter's journal: the guard row is armed before the first edge, so no
//! staged byte exists without a durable record that will end it. Nothing
//! here severs or moves an edge. The cleanup executor resolves the guard:
//! onto the key's retained record once one is registered, or to nothing once
//! the starter's journal is settled with no record.
//!
//! Every engine runs this one sequence. The local executor runs it inline; the
//! Restate controller runs it inside one journaled step, so a replay of the
//! parent reads the recorded result and never registers again (ADR 0107: the
//! registrar mints an id once per start, and a replay after the process was
//! pruned must still see that id).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{
    ProcessEngineRegistry, ProcessExecutionEnvRef, ProcessExecutionEnvSpec,
    ProcessExecutionEnvStore, ProcessInput, ProcessRecord, ProcessRegistration, ProcessRegistry,
    SessionId, StoreRealization, artifact_referrer_ended,
};
use crate::{
    ArtifactCleanupPlan, ArtifactReferrer, ArtifactStoreId, ReferrerClaim,
    RuntimeEffectControllerError, TurnFailureCause,
};

/// The stores one process start writes through.
pub struct ProcessStartStores<'a> {
    pub registry: &'a dyn ProcessRegistry,
    pub env_store: Option<&'a Arc<dyn ProcessExecutionEnvStore>>,
    pub engines: Option<&'a ProcessEngineRegistry>,
    /// Whether an engine start with no engine registry is refused (a durable
    /// controller) or registered without staging engine artifacts (the local
    /// executor, whose host may serve engine rows elsewhere).
    pub engines_required: bool,
    /// Names the executor in a refusal, e.g. "Restate process start".
    pub executor: &'static str,
    /// The journal of the scope running the start: the authority of the
    /// start's `AwaitStart` guard (ADR 0113 §3.3).
    pub starter: &'a lash_sansio::EffectJournalIdentity,
}

/// What one process start registered: the result a durable controller
/// records, so a replay reads it instead of registering again.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredProcessStart {
    /// The registered row: the one this start created, or the one the
    /// registrar retains under the start key.
    pub record: ProcessRecord,
    /// Whether the registrar created the row for this start.
    pub disposition: crate::ProcessRegistrationDisposition,
    /// The execution environment the start's registration names once staged.
    pub env_ref: Option<ProcessExecutionEnvRef>,
}

impl RegisteredProcessStart {
    /// The registry's verdict: a coalesced start is reported as a replay
    /// rather than a fresh start (FIG-3070).
    pub fn realization(&self) -> StoreRealization {
        StoreRealization::from_wrote(
            self.disposition == crate::ProcessRegistrationDisposition::Created,
        )
    }
}

/// Stages and registers one process start.
///
/// A start is addressed by its key (ADR 0107); a start with none is refused.
/// Everything the start names is staged under `Start(key)` first. A start
/// whose key is already fenced — an earlier attempt settled it — stages
/// nothing there and, once the registrar returns the row, acquires its own
/// content directly under `ProcessRecord(id)` where the row adopts it.
///
/// # Errors
///
/// A refusal for a start with no key or an executor missing a store the
/// start needs, and any store failure.
pub async fn register_process_start(
    stores: &ProcessStartStores<'_>,
    mut registration: ProcessRegistration,
    observers: &[SessionId],
    env_spec: Option<&ProcessExecutionEnvSpec>,
) -> Result<RegisteredProcessStart, RuntimeEffectControllerError> {
    let Some(start_key) = registration.start_key.clone() else {
        return Err(RuntimeEffectControllerError::foreign(
            "process_start_key_missing",
            TurnFailureCause::Outcome,
            "a journaled process start must carry its start key",
        ));
    };
    let claim = ReferrerClaim::guarded(
        ArtifactReferrer::Start(start_key),
        ArtifactCleanupPlan::AwaitStart {
            starter: stores.starter.clone(),
        },
    )
    .map_err(|error| crate::PluginError::Session(error.to_string()))?;
    let env = stage_env(stores, &claim, &mut registration, env_spec).await?;
    let engine = stage_engine(stores, &claim, &registration).await?;
    let submitted_env_ref = registration.env_ref.clone();
    let submitted_input = Arc::clone(&registration.input);
    let registered = stores
        .registry
        .register_process_reporting_disposition(registration, observers)
        .await?;
    let disposition = registered.disposition;
    let created = disposition == crate::ProcessRegistrationDisposition::Created;
    let record = registered.record;
    let process_claim =
        ReferrerClaim::unguarded(ArtifactReferrer::ProcessRecord(record.id.clone()))
            .map_err(|error| crate::PluginError::Session(error.to_string()))?;
    if let (Some(env_store), Some(env)) = (stores.env_store, env.as_ref())
        && !env.staged
        && (created || record.env_ref == submitted_env_ref)
    {
        acquire_env(env_store.as_ref(), &process_claim, env).await?;
    }
    if let Some(engine) = engine.as_ref()
        && !engine.staged
        && (created || record.input == submitted_input)
    {
        acquire_engine_names(engine, &process_claim).await?;
    }
    Ok(RegisteredProcessStart {
        record,
        disposition,
        env_ref: submitted_env_ref,
    })
}

async fn acquire_env(
    env_store: &dyn ProcessExecutionEnvStore,
    claim: &ReferrerClaim,
    env: &StagedEnv,
) -> Result<bool, RuntimeEffectControllerError> {
    let acquired = match &env.bytes {
        Some(bytes) => {
            env_store
                .publish_process_execution_env(claim, &env.env_ref, bytes)
                .await
        }
        None => {
            env_store
                .acquire_process_execution_env(claim, &env.env_ref)
                .await
        }
    };
    match acquired.map_err(crate::PluginError::from) {
        Ok(()) => Ok(true),
        Err(error) if artifact_referrer_ended(&error) == Some(claim.referrer()) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

async fn acquire_engine_names(
    engine: &StagedEngine,
    claim: &ReferrerClaim,
) -> Result<bool, RuntimeEffectControllerError> {
    for artifact_ref in &engine.engine_refs {
        match engine
            .engine
            .acquire_engine_artifact(claim, artifact_ref)
            .await
        {
            Ok(()) => {}
            Err(error) if artifact_referrer_ended(&error) == Some(claim.referrer()) => {
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(true)
}

async fn stage_env(
    stores: &ProcessStartStores<'_>,
    claim: &ReferrerClaim,
    registration: &mut ProcessRegistration,
    env_spec: Option<&ProcessExecutionEnvSpec>,
) -> Result<Option<StagedEnv>, RuntimeEffectControllerError> {
    let missing_store = |what: &str| {
        RuntimeEffectControllerError::foreign(
            "process_env_store_unavailable",
            TurnFailureCause::Outcome,
            format!(
                "admitted {} {what} an execution environment but the executor has no environment store",
                stores.executor
            ),
        )
    };
    let mut env = if let Some(env_spec) = env_spec {
        let encode_error = |error: serde_json::Error| {
            crate::PluginError::Session(format!(
                "failed to encode process execution environment: {error}"
            ))
        };
        let env_ref = env_spec.stable_ref().map_err(encode_error)?;
        let bytes = env_spec.to_store_bytes().map_err(encode_error)?;
        *registration = registration
            .clone()
            .with_execution_env_ref(Some(env_ref.clone()));
        StagedEnv {
            env_ref,
            bytes: Some(bytes),
            staged: false,
        }
    } else if let Some(env_ref) = registration.env_ref.clone() {
        // An existing or inherited environment is acquired, never borrowed:
        // the start's own edge is what keeps it alive (ADR 0113 §3.3).
        StagedEnv {
            env_ref,
            bytes: None,
            staged: false,
        }
    } else {
        return Ok(None);
    };
    let env_store = stores.env_store.ok_or_else(|| {
        missing_store(if env.bytes.is_some() {
            "carries"
        } else {
            "references"
        })
    })?;
    env.staged = acquire_env(env_store.as_ref(), claim, &env).await?;
    Ok(Some(env))
}

async fn stage_engine(
    stores: &ProcessStartStores<'_>,
    claim: &ReferrerClaim,
    registration: &ProcessRegistration,
) -> Result<Option<StagedEngine>, RuntimeEffectControllerError> {
    let ProcessInput::Engine { kind, payload } = registration.input.as_ref() else {
        return Ok(None);
    };
    let Some(engines) = stores.engines else {
        if stores.engines_required {
            return Err(RuntimeEffectControllerError::foreign(
                "process_engine_registry_unavailable",
                TurnFailureCause::Outcome,
                format!(
                    "admitted {} requires an engine but the executor has no process-engine registry",
                    stores.executor
                ),
            ));
        }
        return Ok(None);
    };
    let engine = engines.require(kind)?;
    let own_store = ArtifactStoreId::Engine(kind.clone());
    let engine_refs = engine
        .start_artifacts(payload)?
        .into_iter()
        .filter(|name| name.store == own_store)
        .map(|name| name.artifact_ref)
        .collect();
    let mut staged = StagedEngine {
        engine,
        engine_refs,
        staged: false,
    };
    staged.staged = acquire_engine_names(&staged, claim).await?;
    Ok(Some(staged))
}

struct StagedEnv {
    env_ref: ProcessExecutionEnvRef,
    /// The bytes a start that carries its spec publishes; `None` for an
    /// existing or inherited reference, which is acquired.
    bytes: Option<Vec<u8>>,
    /// Whether the start's own referrer holds it: `false` when `Start(key)`
    /// was already fenced.
    staged: bool,
}

struct StagedEngine {
    engine: Arc<dyn super::ProcessEngine>,
    /// The start's names in the engine's own store.
    engine_refs: Vec<String>,
    staged: bool,
}
