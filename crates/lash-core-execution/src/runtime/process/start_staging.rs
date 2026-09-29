//! The store half of one process start: stage what the start carries under
//! the start's referrer, then register the row (ADR 0113 §3.3).
//!
//! Staging is an acquisition of `Start(key)`, guarded by `AwaitStart` on the
//! starter's journal: the guard row is armed before the first edge, so no
//! staged byte exists without a durable record that will end it. Nothing
//! here severs or moves an edge. The cleanup executor resolves the guard:
//! onto the key's retained record once one is registered, or to nothing once
//! the starter's journal is settled with no record. A terminal refusal before
//! any process holds the key ends `Start(key)` at once.
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
    ArtifactCleanup, ArtifactCleanupPlan, ArtifactName, ArtifactReferrer, ArtifactStoreId,
    ModuleArtifactStore, ReferrerClaim, RuntimeEffectControllerError, StartKey, TurnFailureCause,
    runtime::Clock, store::ArtifactCleanupLedger,
};

/// The artifact stores a referrer acquires through, and the cleanup ledger
/// its guards arm in (ADR 0113 §2.1, §2.4): what a process start, a trigger
/// revision and a definition revision need to hold every artifact a
/// [`ProcessEngine::start_artifacts`](super::ProcessEngine::start_artifacts)
/// name points at, whichever store holds it.
#[derive(Clone)]
pub struct ArtifactReferrerPorts {
    modules: Arc<dyn ModuleArtifactStore>,
    env: Arc<dyn ProcessExecutionEnvStore>,
    cleanup: Arc<dyn ArtifactCleanupLedger>,
    clock: Arc<dyn Clock>,
}

/// Whether an acquisition added the claim's edges or met its fence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferrerAcquisition {
    /// The claim's referrer holds every name.
    Held,
    /// The claim's referrer has a fence: it ended before this acquisition,
    /// and holds nothing it did not already hold.
    Ended,
}

impl ArtifactReferrerPorts {
    pub fn new(
        modules: Arc<dyn ModuleArtifactStore>,
        env: Arc<dyn ProcessExecutionEnvStore>,
        cleanup: Arc<dyn ArtifactCleanupLedger>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            modules,
            env,
            cleanup,
            clock,
        }
    }

    /// The ports of `backend`'s store set.
    pub fn of_backend(backend: &crate::Backend) -> Self {
        Self::new(
            backend.module_artifacts(),
            backend.process_env_store(),
            backend.artifact_cleanup(),
            backend.clock(),
        )
    }

    pub fn modules(&self) -> &Arc<dyn ModuleArtifactStore> {
        &self.modules
    }

    pub fn env(&self) -> &Arc<dyn ProcessExecutionEnvStore> {
        &self.env
    }

    pub fn cleanup(&self) -> &Arc<dyn ArtifactCleanupLedger> {
        &self.cleanup
    }

    /// Add the claim's edge to every name, each in the store that holds it.
    ///
    /// Store-set names go first: their own transaction arms the claim's
    /// guard. Before the first engine-store name, the guard is armed in the
    /// ledger, since an engine store cannot write it (ADR 0113 §2.2). A fence
    /// on the claim's referrer stops the acquisition and answers
    /// [`ReferrerAcquisition::Ended`].
    ///
    /// # Errors
    ///
    /// A name under an engine `engines` does not have, `ArtifactMissing`, and
    /// every store failure.
    pub async fn acquire(
        &self,
        engines: &ProcessEngineRegistry,
        claim: &ReferrerClaim,
        names: &[ArtifactName],
    ) -> Result<ReferrerAcquisition, crate::PluginError> {
        let (engine_names, store_names): (Vec<&ArtifactName>, Vec<&ArtifactName>) = names
            .iter()
            .partition(|name| matches!(name.store, ArtifactStoreId::Engine(_)));
        for name in store_names {
            let acquired = match &name.store {
                ArtifactStoreId::LashlangModule => {
                    self.modules
                        .acquire_module_artifact(claim, &name.artifact_ref)
                        .await
                }
                ArtifactStoreId::ProcessEnv => {
                    self.env
                        .acquire_process_execution_env(
                            claim,
                            &ProcessExecutionEnvRef::new(name.artifact_ref.clone()),
                        )
                        .await
                }
                ArtifactStoreId::Engine(_) => continue,
            };
            if let Some(ended) = held_or_ended(claim, acquired.map_err(crate::PluginError::from))? {
                return Ok(ended);
            }
        }
        if engine_names.is_empty() {
            return Ok(ReferrerAcquisition::Held);
        }
        if let Some(guard) = claim.guard_cleanup() {
            self.arm(&guard).await?;
        }
        for name in engine_names {
            let ArtifactStoreId::Engine(kind) = &name.store else {
                continue;
            };
            let acquired = engines
                .require(kind)?
                .acquire_engine_artifact(claim, &name.artifact_ref)
                .await;
            if let Some(ended) = held_or_ended(claim, acquired)? {
                return Ok(ended);
            }
        }
        Ok(ReferrerAcquisition::Held)
    }

    /// Upsert `cleanup` under ADR 0113 §2.4's rule, due now.
    ///
    /// # Errors
    ///
    /// The ledger's failure.
    pub async fn arm(&self, cleanup: &ArtifactCleanup) -> Result<(), crate::PluginError> {
        self.cleanup
            .arm_cleanup(cleanup, self.clock.timestamp_ms())
            .await
            .map(|_| ())
            .map_err(|error| crate::PluginError::Session(error.to_string()))
    }

    /// End `referrer` now: its `Ended` record with no carries and no gate.
    ///
    /// # Errors
    ///
    /// The ledger's failure.
    pub async fn end(&self, referrer: ArtifactReferrer) -> Result<(), crate::PluginError> {
        self.arm(&ArtifactCleanup::ended(referrer, Vec::new(), None))
            .await
    }

    /// Make `referrer`'s cleanup due now, after the fact that ends it
    /// committed elsewhere. A nudge only shortens a guard's wait and decides
    /// nothing, so a failed one is logged and dropped: the guard still
    /// resolves on its own cadence.
    pub async fn nudge(&self, referrer: &ArtifactReferrer) {
        if let Err(error) = self
            .cleanup
            .nudge(referrer, self.clock.timestamp_ms())
            .await
        {
            tracing::warn!(%referrer, %error, "artifact cleanup nudge failed");
        }
    }
}

/// `Some(Ended)` when `acquired` failed on the claim's own fence, `None` when
/// it held, and the error otherwise.
fn held_or_ended(
    claim: &ReferrerClaim,
    acquired: Result<(), crate::PluginError>,
) -> Result<Option<ReferrerAcquisition>, crate::PluginError> {
    match acquired {
        Ok(()) => Ok(None),
        Err(error) if artifact_referrer_ended(&error) == Some(claim.referrer()) => {
            Ok(Some(ReferrerAcquisition::Ended))
        }
        Err(error) => Err(error),
    }
}

/// The stores one process start writes through.
pub struct ProcessStartStores<'a> {
    pub registry: &'a dyn ProcessRegistry,
    pub env_store: Option<&'a Arc<dyn ProcessExecutionEnvStore>>,
    /// The engines a start names artifacts through, and the
    /// [`ArtifactReferrerPorts`] that hold them.
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

impl ProcessStartStores<'_> {
    fn ports(&self) -> Option<&ArtifactReferrerPorts> {
        self.engines.and_then(ProcessEngineRegistry::artifact_ports)
    }
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
/// Once the row commits, the start nudges `Start(key)`'s guard, which then
/// carries the retained row's content onto `ProcessRecord(id)`. A terminal
/// refusal while no process holds the key upserts `Start(key)`'s `Ended`
/// record instead: the authoritative abandonment (ADR 0113 §3.3).
///
/// # Errors
///
/// A refusal for a start with no key or an executor missing a store the
/// start needs, and any store failure.
pub async fn register_process_start(
    stores: &ProcessStartStores<'_>,
    registration: ProcessRegistration,
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
    match stage_and_register(stores, &start_key, registration, observers, env_spec).await {
        Ok(registered) => {
            if let Some(ports) = stores.ports() {
                ports.nudge(&ArtifactReferrer::Start(start_key)).await;
            }
            Ok(registered)
        }
        Err(error) => {
            if error.is_terminal() {
                abandon_start(stores, start_key).await?;
            }
            Err(error)
        }
    }
}

/// End `Start(key)` after a terminal refusal, unless a process already holds
/// the key: then the registered row is the start's outcome, and its guard
/// carries onto it.
///
/// A key is global, so another start may be staged under `Start(key)` while
/// this one is refused (FIG-4111). Its row can commit between the read below
/// and the end: `Start(key)` then carries nothing onto it. Either that start
/// meets the fence once its row commits and holds its own content under
/// `ProcessRecord` ([`stage_and_register`]), or it looked before the fence
/// existed, and then the read after the end finds its row and holds the row's
/// content here.
async fn abandon_start(
    stores: &ProcessStartStores<'_>,
    start_key: StartKey,
) -> Result<(), RuntimeEffectControllerError> {
    let Some(ports) = stores.ports() else {
        return Ok(());
    };
    if stores
        .registry
        .get_process_by_start_key(&start_key)
        .await?
        .is_some()
    {
        return Ok(());
    }
    ports
        .end(ArtifactReferrer::Start(start_key.clone()))
        .await?;
    if let Some(retained) = stores.registry.get_process_by_start_key(&start_key).await?
        && let Err(error) = hold_retained_start(stores, ports, &retained).await
    {
        // This start's refusal stands either way; the row is another start's.
        tracing::warn!(
            process_id = %retained.id,
            %error,
            "could not hold a concurrent start's content after abandoning its key"
        );
    }
    Ok(())
}

/// Hold `retained`'s environment and engine artifacts under its
/// `ProcessRecord`: what `Start(key)`'s guard would have carried onto it.
async fn hold_retained_start(
    stores: &ProcessStartStores<'_>,
    ports: &ArtifactReferrerPorts,
    retained: &ProcessRecord,
) -> Result<(), RuntimeEffectControllerError> {
    let claim = ReferrerClaim::unguarded(ArtifactReferrer::ProcessRecord(retained.id.clone()))
        .map_err(|error| crate::PluginError::Session(error.to_string()))?;
    if let (Some(env_store), Some(env_ref)) = (stores.env_store, retained.env_ref.as_ref()) {
        let env = StagedEnv {
            env_ref: env_ref.clone(),
            bytes: None,
            staged: false,
        };
        acquire_env(env_store.as_ref(), &claim, &env).await?;
    }
    if let (Some(engines), ProcessInput::Engine { kind, payload }) =
        (stores.engines, retained.input.as_ref())
    {
        let names = engines.require(kind)?.start_artifacts(payload)?;
        if !names.is_empty() {
            ports.acquire(engines, &claim, &names).await?;
        }
    }
    Ok(())
}

async fn stage_and_register(
    stores: &ProcessStartStores<'_>,
    start_key: &StartKey,
    mut registration: ProcessRegistration,
    observers: &[SessionId],
    env_spec: Option<&ProcessExecutionEnvSpec>,
) -> Result<RegisteredProcessStart, RuntimeEffectControllerError> {
    let claim = ReferrerClaim::guarded(
        ArtifactReferrer::Start(start_key.clone()),
        ArtifactCleanupPlan::AwaitStart {
            starter: stores.starter.clone(),
        },
    )
    .map_err(|error| crate::PluginError::Session(error.to_string()))?;
    let env = stage_env(stores, &claim, &mut registration, env_spec).await?;
    let engine = stage_engine(stores, &claim, &registration, env.as_ref()).await?;
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
    let adopts_env = created || record.env_ref == submitted_env_ref;
    let adopts_engine = created || record.input == submitted_input;
    // A key is global, so another start's terminal refusal can end
    // `Start(key)` after this start staged there and before its row committed
    // (`abandon_start`, FIG-4111). The guard then carries nothing onto the
    // row: this start holds what it staged under `ProcessRecord` itself.
    let start_ended = (adopts_env || adopts_engine)
        && start_ended_after_staging(stores, &claim, env.as_ref(), engine.as_ref()).await?;
    if let (Some(env_store), Some(env)) = (stores.env_store, env.as_ref())
        && (!env.staged || start_ended)
        && adopts_env
    {
        acquire_env(env_store.as_ref(), &process_claim, env).await?;
    }
    if let Some(engine) = engine.as_ref()
        && (!engine.staged || start_ended)
        && adopts_engine
    {
        engine
            .ports
            .acquire(engine.engines, &process_claim, &engine.names)
            .await?;
    }
    Ok(RegisteredProcessStart {
        record,
        disposition,
        env_ref: submitted_env_ref,
    })
}

/// Whether `Start(key)` was fenced after this start staged under it: one
/// staged name acquired again under `claim` meets the fence. `false` when
/// nothing was staged there.
async fn start_ended_after_staging(
    stores: &ProcessStartStores<'_>,
    claim: &ReferrerClaim,
    env: Option<&StagedEnv>,
    engine: Option<&StagedEngine<'_>>,
) -> Result<bool, RuntimeEffectControllerError> {
    if let (Some(env_store), Some(env)) = (stores.env_store, env)
        && env.staged
    {
        // The bytes are stored: acquire the reference, publish nothing.
        let probe = StagedEnv {
            env_ref: env.env_ref.clone(),
            bytes: None,
            staged: true,
        };
        return Ok(!acquire_env(env_store.as_ref(), claim, &probe).await?);
    }
    if let Some(engine) = engine
        && engine.staged
        && let Some(name) = engine.names.first()
    {
        let acquired = engine
            .ports
            .acquire(engine.engines, claim, std::slice::from_ref(name))
            .await?;
        return Ok(acquired == ReferrerAcquisition::Ended);
    }
    Ok(false)
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
    Ok(held_or_ended(claim, acquired.map_err(crate::PluginError::from))?.is_none())
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

async fn stage_engine<'a>(
    stores: &ProcessStartStores<'a>,
    claim: &ReferrerClaim,
    registration: &ProcessRegistration,
    env: Option<&StagedEnv>,
) -> Result<Option<StagedEngine<'a>>, RuntimeEffectControllerError> {
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
    let names = engines.require(kind)?.start_artifacts(payload)?;
    if names.is_empty() {
        return Ok(None);
    }
    let Some(ports) = engines.artifact_ports() else {
        return Err(RuntimeEffectControllerError::foreign(
            "process_artifact_ports_unavailable",
            TurnFailureCause::Outcome,
            format!(
                "admitted {} names `{kind}` artifacts but the executor's engine registry has no artifact stores to hold them",
                stores.executor
            ),
        ));
    };
    // A start whose environment met `Start(key)`'s fence stages nothing more
    // under it: the key settled before this attempt.
    let staged = match env {
        Some(env) if !env.staged => false,
        _ => ports.acquire(engines, claim, &names).await? == ReferrerAcquisition::Held,
    };
    Ok(Some(StagedEngine {
        engines,
        ports,
        names,
        staged,
    }))
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

struct StagedEngine<'a> {
    engines: &'a ProcessEngineRegistry,
    ports: &'a ArtifactReferrerPorts,
    /// Every artifact the start payload names, in any store.
    names: Vec<ArtifactName>,
    staged: bool,
}
