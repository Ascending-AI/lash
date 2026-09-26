//! The store half of one process start: stage what the start carries, register
//! the row, and settle the staging onto the process the registrar returned.
//!
//! Every engine runs this one sequence. The local executor runs it inline; the
//! Restate controller runs it inside one journaled step, so a replay of the
//! parent reads the recorded result and never registers again (ADR 0107: the
//! registrar mints an id once per start, and a replay after the process was
//! pruned must still see that id).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{
    ArtifactOwner, ProcessEngine, ProcessEngineRegistry, ProcessExecutionEnvRef,
    ProcessExecutionEnvSpec, ProcessExecutionEnvStore, ProcessInput, ProcessRecord,
    ProcessRegistration, ProcessRegistry, SessionId, StoreRealization,
    artifact_owner_is_permanently_retired, publish_process_execution_env,
    settle_started_process_engine_artifacts, settle_started_process_execution_env,
};
use crate::{ProcessCommand, RuntimeEffectControllerError, TurnFailureCause};

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
    pub created: bool,
    /// The execution environment the start's registration names once staged.
    pub env_ref: Option<ProcessExecutionEnvRef>,
}

impl RegisteredProcessStart {
    /// The registry's verdict: a coalesced start is reported as a replay
    /// rather than a fresh start (FIG-3070).
    pub fn realization(&self) -> StoreRealization {
        StoreRealization::from_wrote(self.created)
    }
}

struct StagedEnv {
    env_ref: ProcessExecutionEnvRef,
    bytes: Vec<u8>,
    staged: bool,
}

struct StagedEngine {
    engine: Arc<dyn ProcessEngine>,
    payload: serde_json::Value,
    staged: bool,
}

/// Stages, registers and settles one process start.
///
/// A start is addressed by its key (ADR 0107); a start with none is refused.
/// The key is trusted: a start that finds the key's retained process returns
/// it, adopting this attempt's staging only where it is the retained content.
/// What this attempt staged and the retained process did not adopt is
/// released, but only after whatever the retained process names has been
/// secured under the process's own owner: the staging owner is shared by
/// every attempt at one key, and an earlier attempt that crashed before
/// settling left the retained process's content on it.
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
    if registration.start_key.is_none() {
        return Err(RuntimeEffectControllerError::foreign(
            "process_start_key_missing",
            TurnFailureCause::Outcome,
            "a journaled process start must carry its start key",
        ));
    }
    let start_effect_id = ProcessCommand::start_effect_id(registration.start_key.as_ref());
    let staging_owner = ArtifactOwner::process_start(&start_effect_id);
    let env = stage_env(stores, &staging_owner, &mut registration, env_spec).await?;
    let engine = stage_engine(stores, &staging_owner, &registration).await?;
    let submitted_env_ref = registration.env_ref.clone();
    let submitted_input = Arc::clone(&registration.input);
    let registered = match stores
        .registry
        .register_process_reporting_disposition(registration, observers)
        .await
    {
        Ok(registered) => registered,
        Err(error) => {
            // The staging owner is shared by every attempt at the key, so it
            // is retired only when nothing registered can reference it: a
            // refusal taken before any process holds the key. A fault leaves
            // it for the retry, and a content conflict means a retained
            // process holds the key and may still settle from it.
            if !error.is_terminal() || crate::is_durable_identity_conflict(&error) {
                return Err(error.into());
            }
            if let Some(env_store) = stores.env_store {
                env_store
                    .retire_process_execution_env_owner(&staging_owner)
                    .await?;
            }
            if let Some(engine) = engine.as_ref() {
                engine.engine.retire_artifact_owner(&staging_owner).await?;
            }
            return Err(error.into());
        }
    };
    let created = registered.is_created();
    let record = registered.record;
    let process_owner = ArtifactOwner::process(record.id.clone());
    let adopts_env = created || record.env_ref == submitted_env_ref;
    let adopts_engine = created || record.input == submitted_input;
    if let (Some(env_store), Some(env)) = (stores.env_store, env.as_ref()) {
        if adopts_env {
            settle_started_process_execution_env(
                env_store.as_ref(),
                &staging_owner,
                &process_owner,
                &env.env_ref,
                &env.bytes,
                env.staged,
            )
            .await?;
        } else {
            secure_retained_env(env_store.as_ref(), &process_owner, &record).await?;
            env_store
                .retire_process_execution_env_owner(&staging_owner)
                .await?;
        }
    }
    if let Some(engine) = engine.as_ref() {
        if adopts_engine {
            settle_started_process_engine_artifacts(
                engine.engine.as_ref(),
                &staging_owner,
                &process_owner,
                &engine.payload,
                engine.staged,
            )
            .await?;
        } else {
            secure_retained_engine_artifacts(engine.engine.as_ref(), &process_owner, &record)
                .await?;
            engine.engine.retire_artifact_owner(&staging_owner).await?;
        }
    }
    Ok(RegisteredProcessStart {
        record,
        created,
        env_ref: submitted_env_ref,
    })
}

/// Protects the retained process's own environment under the process owner
/// before the shared staging owner is retired: an earlier attempt that
/// registered the process and crashed before settling left it on the staging
/// owner alone.
async fn secure_retained_env(
    env_store: &dyn ProcessExecutionEnvStore,
    process_owner: &ArtifactOwner,
    record: &ProcessRecord,
) -> Result<(), RuntimeEffectControllerError> {
    let Some(retained) = record.env_ref.as_ref() else {
        return Ok(());
    };
    let Some(bytes) = env_store.get_process_execution_env(retained).await? else {
        return Ok(());
    };
    match env_store
        .publish_process_execution_env(process_owner, retained, &bytes)
        .await
    {
        Ok(()) => Ok(()),
        // A retired process owner holds nothing to protect any more.
        Err(error) if artifact_owner_is_permanently_retired(&error) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// The engine half of [`secure_retained_env`].
async fn secure_retained_engine_artifacts(
    engine: &dyn ProcessEngine,
    process_owner: &ArtifactOwner,
    record: &ProcessRecord,
) -> Result<(), RuntimeEffectControllerError> {
    let ProcessInput::Engine { kind, payload } = record.input.as_ref() else {
        return Ok(());
    };
    if kind != engine.kind() {
        return Ok(());
    }
    match engine.protect_start_artifacts(process_owner, payload).await {
        Ok(()) => Ok(()),
        Err(error) if artifact_owner_is_permanently_retired(&error) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn stage_env(
    stores: &ProcessStartStores<'_>,
    staging_owner: &ArtifactOwner,
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
    if let Some(env_spec) = env_spec {
        let env_store = stores.env_store.ok_or_else(|| missing_store("carries"))?;
        let encode_error = |error: serde_json::Error| {
            crate::PluginError::Session(format!(
                "failed to encode process execution environment: {error}"
            ))
        };
        let expected_ref = env_spec.stable_ref().map_err(encode_error)?;
        let bytes = env_spec.to_store_bytes().map_err(encode_error)?;
        let (env_ref, staged) = match publish_process_execution_env(
            env_store.as_ref(),
            staging_owner,
            env_spec,
        )
        .await
        {
            Ok(env_ref) => (env_ref, true),
            Err(error) if artifact_owner_is_permanently_retired(&error) => (expected_ref, false),
            Err(error) => return Err(error.into()),
        };
        *registration = registration
            .clone()
            .with_execution_env_ref(Some(env_ref.clone()));
        return Ok(Some(StagedEnv {
            env_ref,
            bytes,
            staged,
        }));
    }
    let Some(env_ref) = registration.env_ref.clone() else {
        return Ok(None);
    };
    let env_store = stores
        .env_store
        .ok_or_else(|| missing_store("references"))?;
    let bytes = env_store
        .get_process_execution_env(&env_ref)
        .await?
        .ok_or_else(|| {
            crate::PluginError::Session(format!("missing process execution env `{env_ref}`"))
        })?;
    let staged = match env_store
        .publish_process_execution_env(staging_owner, &env_ref, &bytes)
        .await
    {
        Ok(()) => true,
        Err(error) if artifact_owner_is_permanently_retired(&error) => false,
        Err(error) => return Err(error.into()),
    };
    Ok(Some(StagedEnv {
        env_ref,
        bytes,
        staged,
    }))
}

async fn stage_engine(
    stores: &ProcessStartStores<'_>,
    staging_owner: &ArtifactOwner,
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
    let staged = match engine.protect_start_artifacts(staging_owner, payload).await {
        Ok(()) => true,
        Err(error) if artifact_owner_is_permanently_retired(&error) => false,
        Err(error) => return Err(error.into()),
    };
    Ok(Some(StagedEngine {
        engine,
        payload: payload.clone(),
        staged,
    }))
}
