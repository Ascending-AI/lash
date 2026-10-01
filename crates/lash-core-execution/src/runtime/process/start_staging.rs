//! The store half of one process start: stage what the start carries under
//! the start's referrer, then register the row (ADR 0113 §3.3).
//!
//! Engine staging acquires `Start(key)`, guarded by `AwaitStart` on the
//! starter's journal: the guard row is armed before the first edge, so no
//! staged byte exists without a durable record that will end it. Nothing
//! here severs or moves an edge. The cleanup executor resolves the guard:
//! onto the key's retained record once one is registered, or to nothing once
//! the starter's journal is settled with no record. A terminal refusal before
//! any process holds the key ends `Start(key)` at once.
//!
//! Input attachments acquire `StartInput(key, starter)` before registration.
//! Its guard also awaits the retained start or the starter's settled journal;
//! cleanup holds the retained input before ending staging. The starter makes
//! this claim independent of earlier uses of a pruned host key.
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
/// revision and a definition publication need to hold every artifact a
/// [`ProcessEngine::start_artifacts`](super::ProcessEngine::start_artifacts)
/// name points at, whichever store holds it.
#[derive(Clone)]
pub struct ArtifactReferrerPorts {
    modules: Arc<dyn ModuleArtifactStore>,
    env: Arc<dyn ProcessExecutionEnvStore>,
    definitions: Arc<dyn super::ProcessDefinitionStore>,
    attachments: Arc<dyn crate::AttachmentReferrers>,
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
        definitions: Arc<dyn super::ProcessDefinitionStore>,
        attachments: Arc<dyn crate::AttachmentReferrers>,
        cleanup: Arc<dyn ArtifactCleanupLedger>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            modules,
            env,
            definitions,
            attachments,
            cleanup,
            clock,
        }
    }

    /// The ports of `backend`'s store set.
    pub fn of_backend(backend: &crate::Backend) -> Self {
        Self::new(
            backend.module_artifacts(),
            backend.process_env_store(),
            backend.definition_store(),
            backend.attachment_referrers(),
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

    pub fn definitions(&self) -> &Arc<dyn super::ProcessDefinitionStore> {
        &self.definitions
    }

    pub fn attachments(&self) -> &Arc<dyn crate::AttachmentReferrers> {
        &self.attachments
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
                // A descriptor name alone: its manifest is held through
                // `acquire_definition`, which reads it.
                ArtifactStoreId::ProcessDefinition => {
                    let id =
                        super::ProcessDefinitionId::parse(&name.artifact_ref).map_err(|error| {
                            crate::PluginError::Session(format!(
                                "artifact `{}` is not a process definition id: {error}",
                                name.artifact_ref
                            ))
                        })?;
                    self.definitions
                        .acquire_process_definition(claim, &id, &[])
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
            .map_err(crate::PluginError::from)
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
    /// The session catalog a root start's host session-lookup grant is
    /// checked against, inside the recorded admission this registration
    /// runs in. `None` refuses a host-granted start.
    pub session_catalog: Option<&'a dyn crate::store::RuntimeStore>,
    /// Names the executor in a refusal, e.g. "Restate process start".
    pub executor: &'static str,
    /// The journal of the scope running the start: the authority of the
    /// start's `AwaitStart` guard (ADR 0113 §3.3).
    pub starter: &'a lash_sansio::EffectJournalIdentity,
    /// The captured provider route a trigger delivery's start restores
    /// inside the recorded admission this registration runs in. `None` for
    /// every other start, and for a delivery with nothing to restore.
    pub trigger_route: Option<&'a crate::TriggerRouteRestore>,
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
    pub disposition: crate::ProcessRegistrationOutcome,
    /// The execution environment the start's registration names once staged.
    pub env_ref: Option<ProcessExecutionEnvRef>,
}

impl RegisteredProcessStart {
    /// The registry's verdict: a coalesced start is reported as a replay
    /// rather than a fresh start (FIG-3070).
    pub fn realization(&self) -> StoreRealization {
        StoreRealization::from_wrote(self.disposition == crate::ProcessRegistrationOutcome::Created)
    }

    /// The registration the started process runs, from the one the start
    /// submitted. A start by id is resolved at realization (ADR 0113 §3.6):
    /// the process runs the engine input, identity and event types its
    /// record holds, never the unresolved id. Any other start runs what it
    /// submitted. Every start runs under the configuration its record holds.
    #[must_use]
    pub fn running_registration(
        &self,
        mut registration: ProcessRegistration,
    ) -> ProcessRegistration {
        registration.engine_config = self.record.engine_config.clone();
        if !matches!(registration.input.as_ref(), ProcessInput::Definition { .. }) {
            return registration;
        }
        let mut resolved = registration;
        resolved.input = Arc::clone(&self.record.input);
        resolved.identity = self.record.identity.clone();
        resolved.event_types = self.record.event_types.clone();
        resolved
    }
}

/// Stages and registers one process start.
///
/// A start is addressed by its key (ADR 0107); a start with none is refused.
/// Engine artifacts are staged under `Start(key)` first. Input attachments
/// use `StartInput(key, starter)`, so a later use of a pruned host key gets
/// its own staging claim. A start
/// whose key is already fenced — an earlier attempt settled it — stages
/// nothing there and, once the registrar returns the row, acquires its own
/// content directly under `ProcessRecord(id)` where the row adopts it.
///
/// Once the row commits, the start nudges `Start(key)`'s guard, which then
/// carries the retained row's content onto `ProcessRecord(id)`. A terminal
/// refusal while no process holds the key upserts `Start(key)`'s `Ended`
/// record instead: the authoritative abandonment (ADR 0113 §3.3).
/// Input staging is nudged too; without a record its guard waits for the
/// starter to settle, so a refused attempt cannot fence another starter's input.
///
/// # Errors
///
/// A refusal for a start with no key or an executor missing a store the
/// start needs, and any store failure.
pub async fn register_process_start(
    stores: &ProcessStartStores<'_>,
    registration: ProcessRegistration,
    observers: &[SessionId],
) -> Result<RegisteredProcessStart, RuntimeEffectControllerError> {
    let Some(start_key) = registration.start_key.clone() else {
        return Err(RuntimeEffectControllerError::foreign(
            "process_start_key_missing",
            TurnFailureCause::Outcome,
            "a journaled process start must carry its start key",
        ));
    };
    require_host_session_live(stores, &registration).await?;
    restore_trigger_route(stores, &start_key).await?;
    match stage_and_register(stores, &start_key, registration, observers).await {
        Ok(registered) => {
            if let Some(ports) = stores.ports() {
                ports
                    .nudge(&ArtifactReferrer::Start(start_key.clone()))
                    .await;
                ports.nudge(&start_input_referrer(stores, &start_key)).await;
            }
            Ok(registered)
        }
        Err(error) => {
            if error.is_terminal() {
                abandon_start(stores, start_key.clone()).await?;
                if let Some(ports) = stores.ports() {
                    ports.nudge(&start_input_referrer(stores, &start_key)).await;
                }
            }
            Err(error)
        }
    }
}

/// Refuse a root start whose host session-lookup grant names a session the
/// catalog does not hold live.
///
/// The check belongs here, inside the start's recorded admission, and never
/// ahead of the command: a replay after the session was deleted reads the
/// recorded registration instead of looking the session up again, so it
/// answers the start its first run made (ADR 0105 §1). The refusal is
/// terminal, so the admission records it too.
async fn require_host_session_live(
    stores: &ProcessStartStores<'_>,
    registration: &ProcessRegistration,
) -> Result<(), RuntimeEffectControllerError> {
    let super::model::LifetimeDecision::Until {
        scope: super::model::ScopeId::Session(session_id),
        grant: super::model::ScopeGrant::HostSessionLookup,
    } = &registration.lifetime
    else {
        return Ok(());
    };
    let live = match stores.session_catalog {
        Some(catalog) => crate::runtime::session_is_live(catalog, session_id)
            .await
            .map_err(RuntimeEffectControllerError::from)?,
        None => false,
    };
    if live {
        return Ok(());
    }
    Err(RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::HostSessionNotLive,
        format!(
            "{}: {} holds a host session-lookup grant for session `{session_id}`, which the \
             catalog does not hold live",
            stores.executor,
            registration.refusal_name()
        ),
    ))
}

/// Ask the host to restore a trigger delivery's captured provider route,
/// for a start no process holds yet (FIG-4554).
///
/// The restorer is a live host service, so it is asked here, inside the
/// start's recorded admission, and never ahead of the command: a replay
/// after the route was revoked reads the recorded registration instead of
/// asking again. A refusal is the admission's outcome, recorded with its
/// class, so a replay after the route came back reproduces it; the
/// reservation stays owed, and its recovery starts it.
///
/// A process already holding the key is this start, registered by an attempt
/// that never recorded it or by the delivery's first drive. The restorer
/// serves new work only, so the retained start is served unasked.
async fn restore_trigger_route(
    stores: &ProcessStartStores<'_>,
    start_key: &StartKey,
) -> Result<(), RuntimeEffectControllerError> {
    let Some(route) = stores.trigger_route else {
        return Ok(());
    };
    if stores
        .registry
        .get_process_by_start_key(start_key)
        .await?
        .is_some()
    {
        return Ok(());
    }
    let restorer: &dyn crate::TriggerRouteRestorer = route.restorer();
    Ok(restorer.restore(route.capture()).await?)
}

/// End `Start(key)` after a terminal refusal, unless a process already holds
/// the key: then the registered row is the start's outcome, and its guard
/// carries onto it.
///
/// A key is global, so another start may be staged under `Start(key)` while
/// this one is refused (FIG-4111). Its row can commit between the read below
/// and the end, and `Start(key)`'s end then carries nothing onto it. Either
/// that start meets the fence once its row commits and holds its own content
/// under `ProcessRecord` ([`stage_and_register`]), or it looked before the
/// fence existed, and then the cleanup executor, which reads the key's record
/// after the fence, holds the row's content under it before it severs
/// anything (FIG-4130).
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
    ports.end(ArtifactReferrer::Start(start_key)).await?;
    Ok(())
}

async fn stage_and_register(
    stores: &ProcessStartStores<'_>,
    start_key: &StartKey,
    mut registration: ProcessRegistration,
    observers: &[SessionId],
) -> Result<RegisteredProcessStart, RuntimeEffectControllerError> {
    let claim = ReferrerClaim::guarded(
        ArtifactReferrer::Start(start_key.clone()),
        ArtifactCleanupPlan::AwaitStart {
            starter: stores.starter.clone(),
        },
    )
    .map_err(|error| crate::PluginError::Session(error.to_string()))?;
    let env = stage_env(stores, &claim, &registration).await?;
    let env_spec = match env.as_ref() {
        Some(env) => Some(
            super::load_process_execution_env(
                stores
                    .env_store
                    .ok_or_else(|| {
                        crate::PluginError::Session(
                            "process environment store is unavailable".to_string(),
                        )
                    })?
                    .as_ref(),
                &env.env_ref,
            )
            .await
            .map_err(crate::PluginError::from)?,
        ),
        None => None,
    };
    let definition = stage_definition(stores, &claim, &mut registration, env_spec.as_ref()).await?;
    let engine = stage_engine(stores, &claim, &registration, env.as_ref()).await?;
    registration.engine_config = creation_config(stores, &registration, env_spec.as_ref())?;
    Box::pin(stage_input(stores, start_key, registration.input.as_ref())).await?;
    let submitted_env_ref = registration.env_ref.clone();
    let submitted_input = Arc::clone(&registration.input);
    let registered = stores
        .registry
        .register_process_reporting_outcome(registration, observers)
        .await?;
    let disposition = registered.outcome;
    let created = disposition == crate::ProcessRegistrationOutcome::Created;
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
        && start_ended_after_staging(
            stores,
            &claim,
            definition.as_ref(),
            env.as_ref(),
            engine.as_ref(),
        )
        .await?;
    if let Some(definition) = definition.as_ref()
        && (!definition.staged || start_ended)
        && adopts_engine
        && definition
            .ports
            .acquire_definition(definition.engines, &process_claim, &definition.id)
            .await?
            == super::DefinitionAcquisition::Ended
    {
        return Err(RuntimeEffectControllerError::foreign(
            "process_record_ended",
            TurnFailureCause::Outcome,
            format!(
                "process `{}` ended before it could hold definition `{}`",
                record.id, definition.id
            ),
        ));
    }
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
    // StartInput holds the input before registration. Both this step and its
    // cleanup acquire the retained record before staging's attachment edges
    // end, so an abandoned caller leaves the handoff to the cleanup relay.
    if let Some(ports) = stores.ports() {
        crate::runtime::attachment_delivery::acquire_start_input(
            ports.attachments().as_ref(),
            &record,
        )
        .await?;
    }
    Ok(RegisteredProcessStart {
        record,
        disposition,
        env_ref: submitted_env_ref,
    })
}

fn start_input_referrer(stores: &ProcessStartStores<'_>, start_key: &StartKey) -> ArtifactReferrer {
    ArtifactReferrer::StartInput {
        start_key: start_key.clone(),
        starter: stores.starter.clone(),
    }
}

/// Hold input attachments before the registrar publishes a row. A fenced
/// attempt can only replay a retained row, whose own claim is acquired first.
async fn stage_input(
    stores: &ProcessStartStores<'_>,
    start_key: &StartKey,
    input: &ProcessInput,
) -> Result<(), RuntimeEffectControllerError> {
    let ids = input.stored_attachment_ids();
    if ids.is_empty() {
        return Ok(());
    }
    let ports = stores.ports().ok_or_else(|| {
        RuntimeEffectControllerError::foreign(
            "process_start_input_store_unavailable",
            TurnFailureCause::Outcome,
            format!(
                "{} carries stored input attachments but has no attachment stores",
                stores.executor
            ),
        )
    })?;
    let claim = ReferrerClaim::guarded(
        start_input_referrer(stores, start_key),
        ArtifactCleanupPlan::AwaitStart {
            starter: stores.starter.clone(),
        },
    )
    .map_err(|error| crate::PluginError::Session(error.to_string()))?;
    let acquired = ports
        .attachments()
        .acquire_attachment_refs(&claim, &ids)
        .await;
    match acquired {
        Ok(()) => return Ok(()),
        Err(crate::StoreError::ArtifactReferrerEnded { referrer })
            if &referrer == claim.referrer() => {}
        Err(crate::StoreError::UnknownAttachment { digest }) => {
            return Err(RuntimeEffectControllerError::foreign(
                "process_start_input_attachment_unavailable",
                TurnFailureCause::Outcome,
                format!(
                    "{} start input names attachment `{digest}`, which has no upload evidence",
                    stores.executor
                ),
            ));
        }
        Err(error) => return Err(error.into()),
    }
    let retained = stores.registry.get_process_by_start_key(start_key).await?;
    let Some(retained) = retained else {
        return Err(crate::StoreError::ArtifactReferrerEnded {
            referrer: claim.referrer().clone(),
        }
        .into());
    };
    crate::runtime::attachment_delivery::acquire_start_input(
        ports.attachments().as_ref(),
        &retained,
    )
    .await?;
    Ok(())
}

/// Whether `Start(key)` was fenced after this start staged under it: one
/// staged name acquired again under `claim` meets the fence. `false` when
/// nothing was staged there.
async fn start_ended_after_staging(
    stores: &ProcessStartStores<'_>,
    claim: &ReferrerClaim,
    definition: Option<&StagedDefinition<'_>>,
    env: Option<&StagedEnv>,
    engine: Option<&StagedEngine<'_>>,
) -> Result<bool, RuntimeEffectControllerError> {
    if let Some(definition) = definition
        && definition.staged
    {
        let acquired = definition
            .ports
            .acquire_definition(definition.engines, claim, &definition.id)
            .await?;
        return Ok(acquired == super::DefinitionAcquisition::Ended);
    }
    if let (Some(env_store), Some(env)) = (stores.env_store, env)
        && env.staged
    {
        // The bytes are stored: acquire the reference, publish nothing.
        return Ok(!acquire_env(env_store.as_ref(), claim, env).await?);
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
    let acquired = env_store
        .acquire_process_execution_env(claim, &env.env_ref)
        .await;
    Ok(held_or_ended(claim, acquired.map_err(crate::PluginError::from))?.is_none())
}

async fn stage_env(
    stores: &ProcessStartStores<'_>,
    claim: &ReferrerClaim,
    registration: &ProcessRegistration,
) -> Result<Option<StagedEnv>, RuntimeEffectControllerError> {
    let Some(env_ref) = registration.env_ref.clone() else {
        return Ok(None);
    };
    let env_store = stores.env_store.ok_or_else(|| RuntimeEffectControllerError::foreign(
        "process_env_store_unavailable",
        TurnFailureCause::Outcome,
        format!("admitted {} references an execution environment but the executor has no environment store", stores.executor),
    ))?;
    let mut env = StagedEnv {
        env_ref,
        staged: false,
    };
    env.staged = acquire_env(env_store.as_ref(), claim, &env).await?;
    Ok(Some(env))
}

/// What the engine of an engine start records with the row this registration
/// creates (FIG-4527). It is read here, inside the start's one recorded
/// registration step and never ahead of the command: a replay of the start
/// reads the recorded registration, and a start that finds a row retained
/// under its key is returned that row with what its own creation recorded.
/// The start's author states none; a value it carried is replaced.
fn creation_config(
    stores: &ProcessStartStores<'_>,
    registration: &ProcessRegistration,
    env_spec: Option<&ProcessExecutionEnvSpec>,
) -> Result<Option<serde_json::Value>, RuntimeEffectControllerError> {
    let (ProcessInput::Engine { kind, .. }, Some(engines), Some(env_spec)) =
        (registration.input.as_ref(), stores.engines, env_spec)
    else {
        return Ok(None);
    };
    Ok(engines.require(kind)?.creation_config(env_spec)?)
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

/// A start by definition id (ADR 0113 §3.6, the crash table's "before start
/// admission" row): hold the definition's closure under `Start(key)`, have
/// its engine check the descriptor and derive its signature, then admit the
/// engine start it resolves to exactly as every engine start is admitted.
/// The registration then carries that engine input, and its identity names
/// the id, so the key's record holds the descriptor from then on.
///
/// Every refusal happens here, before any row exists: a start of a
/// definition nothing holds, or one its engine refuses, creates no process.
/// A `Start(key)` that is already fenced (an earlier attempt settled the key)
/// holds nothing more: the definition is read, and the row's record holds it
/// once registered.
async fn stage_definition<'a>(
    stores: &'a ProcessStartStores<'a>,
    claim: &ReferrerClaim,
    registration: &mut ProcessRegistration,
    env_spec: Option<&ProcessExecutionEnvSpec>,
) -> Result<Option<StagedDefinition<'a>>, RuntimeEffectControllerError> {
    let ProcessInput::Definition {
        definition_id,
        signature_claim,
        args,
    } = registration.input.as_ref()
    else {
        return Ok(None);
    };
    let (Some(engines), Some(ports)) = (stores.engines, stores.ports()) else {
        return Err(RuntimeEffectControllerError::foreign(
            "process_definition_store_unavailable",
            TurnFailureCause::Outcome,
            format!(
                "admitted {} starts definition `{definition_id}` but the executor has no \
                 definition store to hold it",
                stores.executor
            ),
        ));
    };
    let (resolved, staged) = match ports
        .acquire_definition(engines, claim, definition_id)
        .await?
    {
        super::DefinitionAcquisition::Held(resolved) => (resolved, true),
        super::DefinitionAcquisition::Ended => {
            let Some(resolved) = ports.read_definition(engines, definition_id).await? else {
                return Err(crate::PluginError::Runtime(crate::RuntimeError::new(
                    crate::RuntimeErrorCode::DefinitionMissing,
                    format!(
                        "process definition `{definition_id}` is not stored: no referrer holds it"
                    ),
                ))
                .into());
            };
            (resolved, false)
        }
    };
    if let Some(signature) = signature_claim {
        engines
            .verify_definition_claim(
                &resolved.draft,
                &super::ProcessDefinition::new(definition_id.clone(), signature.clone()),
            )
            .await
            .map_err(|error| {
                crate::PluginError::Runtime(crate::RuntimeError::new(
                    crate::RuntimeErrorCode::DefinitionRefused,
                    error.to_string(),
                ))
            })?;
    }
    let kind = resolved.draft.engine_kind().as_str().to_owned();
    let payload = resolved.start_payload(args)?;
    let declared_label = registration.identity.label.clone();
    let (mut identity, _) = engines.admit(&kind, &payload, env_spec).await?.into_parts();
    let signals = engines
        .resolve(&resolved.draft.unclaimed_reference())
        .await
        .map_err(crate::PluginError::from)?
        .signals;
    identity.definition_id = Some(resolved.id().clone());
    let id = resolved.id().clone();
    let mut resolved_registration = registration.clone();
    resolved_registration.input = Arc::new(ProcessInput::Engine { kind, payload });
    *registration = resolved_registration
        .with_admitted_identity(super::AdmittedProcessIdentity::admitted(identity, signals))
        .with_host_facing_label(declared_label);
    Ok(Some(StagedDefinition {
        engines,
        ports,
        id,
        staged,
    }))
}

struct StagedDefinition<'a> {
    engines: &'a ProcessEngineRegistry,
    ports: &'a ArtifactReferrerPorts,
    id: super::ProcessDefinitionId,
    /// Whether `Start(key)` holds the closure: `false` when it was already
    /// fenced.
    staged: bool,
}

struct StagedEnv {
    env_ref: ProcessExecutionEnvRef,
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
