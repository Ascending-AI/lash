//! The store half of one process start: stage what the start carries under
//! the start's referrer, then register the row (ADR 0113 §3.3).
//!
//! Engine staging acquires `Start(key)`, guarded by `AwaitStart` on the
//! starter's journal: the guard row is armed before the first edge, so no
//! staged byte exists without a durable record that will end it. Nothing
//! here severs or moves an edge. The cleanup executor resolves the guard:
//! onto the key's retained record once one is registered, or to nothing once
//! the starter's journal is settled with no record. A terminal refusal before
//! any process holds the key ends `Start(key)` at once. A start that is its
//! own operation ([`start_operation_journal`]) has no journal to settle:
//! only its registration or its abandonment decides its staging.
//!
//! Input attachments acquire `StartInput(key, starter)` before registration.
//! Its guard also awaits the retained start or the starter's settled journal;
//! cleanup holds the retained input before ending staging. The starter makes
//! this claim independent of earlier uses of a pruned host key.
//!
//! Every start runs this one sequence in three parts: a prepare that admits,
//! stages and mints the registration before any transaction
//! ([`stage_process_start`]); the registrar's transaction that applies the
//! registration; and the adoption of what was staged once the row committed
//! ([`StartStaging::adopt`]).

use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

use super::{
    ProcessEngineRegistry, ProcessExecutionEnvRef, ProcessExecutionEnvSpec,
    ProcessExecutionEnvStore, ProcessInput, ProcessRecord, ProcessRegistration, ProcessRegistry,
    ProcessStartRegistration, ProcessStartTarget, SessionId, StoreRealization,
    artifact_referrer_ended,
};
use crate::{
    ArtifactCleanup, ArtifactName, ArtifactReferrer, ArtifactStoreId, ModuleArtifactStore,
    ReferrerClaim, ReferrerGuard, RuntimeEffectControllerError, StartKey, TurnFailureCause,
    runtime::Clock, store::ArtifactCleanupLedger,
};

/// The artifact stores a referrer acquires through, and the cleanup ledger
/// its guards arm in (ADR 0113 §2.1, §2.4): what a process start and a
/// definition publication need to hold every artifact a
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
                // Tool material is held by Run-segment and source leases only.
                ArtifactStoreId::ToolMaterial => {
                    return Err(crate::PluginError::Session(format!(
                        "a start cannot hold retained tool material `{}`",
                        name.artifact_ref
                    )));
                }
                // A recorded prelude is held by its turn's journal only.
                ArtifactStoreId::TurnPrelude => {
                    return Err(crate::PluginError::Session(format!(
                        "a start cannot hold the recorded turn prelude `{}`",
                        name.artifact_ref
                    )));
                }
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
        Err(error) if artifact_referrer_ended(&error) == Some(&claim.referrer()) => {
            Ok(Some(ReferrerAcquisition::Ended))
        }
        Err(error) => Err(error),
    }
}

/// Admit a host session-turn start before staging writes. `fresh` is false
/// when its key already retains a process, whose model is never revalidated.
/// What the admission publishes it holds under the start's own staging
/// claim, the `Start(key)` claim every other input of the start is staged
/// under (ADR 0113 §3.3).
pub type SessionTurnAdmission = Arc<
    dyn Fn(bool, ReferrerClaim) -> BoxFuture<'static, Result<(), RuntimeEffectControllerError>>
        + Send
        + Sync,
>;

/// What a host start's recorded admission consults beyond the stores every
/// start writes through. Held boxed by the executors that carry it: most
/// process commands are not host starts.
#[derive(Default)]
pub struct HostStartAdmission {
    pub tracing: Option<crate::trace::TraceRuntime>,
    /// The session catalog a root start's host session-lookup grant is
    /// checked against. `None` refuses every host-granted start: nothing can
    /// prove its session live.
    pub session_catalog: Option<Arc<dyn crate::store::RuntimeStore>>,
    /// Validates the child's inherited reasoning before publishing or
    /// acquiring its environment, inside the recorded start admission.
    pub session_turn_admission: Option<SessionTurnAdmission>,
}

/// The stores one process start writes through.
pub struct ProcessStartStores<'a> {
    pub tracing: Option<&'a crate::trace::TraceRuntime>,
    pub registry: &'a dyn ProcessRegistry,
    pub env_store: Option<&'a Arc<dyn ProcessExecutionEnvStore>>,
    /// The engines a start names artifacts through, and the
    /// [`ArtifactReferrerPorts`] that hold them.
    pub engines: &'a ProcessEngineRegistry,
    /// The session catalog a root start's host session-lookup grant is
    /// checked against, inside the recorded admission this registration
    /// runs in. `None` refuses a host-granted start.
    pub session_catalog: Option<&'a dyn crate::store::RuntimeStore>,
    /// A host's child validation and environment publication, executed
    /// before staging and skipped by journal replay.
    pub session_turn_admission: Option<&'a SessionTurnAdmission>,
    /// Names the executor in a refusal, e.g. "local process start".
    pub executor: &'static str,
    /// The journal of the scope running the start: the authority of the
    /// start's `AwaitStart` guard (ADR 0113 §3.3).
    pub starter: &'a lash_sansio::EffectJournalIdentity,
}

impl<'a> ProcessStartStores<'a> {
    fn ports(&self) -> Option<&'a ArtifactReferrerPorts> {
        self.engines.artifact_ports()
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
    /// the process runs the engine input and identity its
    /// record holds, never the unresolved id. Any other start runs what it
    /// submitted. Every start runs under the configuration its record holds.
    #[must_use]
    pub fn running_registration(
        &self,
        registration: impl Into<ProcessStartRegistration>,
    ) -> ProcessRegistration {
        let mut registration = registration.into();
        registration.engine_config = self.record.engine_config.clone();
        match registration.input.as_ref() {
            // The row holds the request with the default binding its
            // registration recorded; the process runs that (FIG-4531).
            ProcessStartTarget::Input(ProcessInput::SessionTurn { .. }) => {
                registration.with_input(Arc::clone(&self.record.input))
            }
            ProcessStartTarget::Input(input @ ProcessInput::Engine { .. }) => {
                let input = Arc::new(input.clone());
                registration.with_input(input)
            }
            ProcessStartTarget::Definition { .. } => {
                let mut resolved = registration.with_input(Arc::clone(&self.record.input));
                resolved.identity = self.record.identity.clone();
                resolved
            }
        }
    }
}

/// Stages and registers one process start: [`stage_process_start`], the
/// registrar's transaction, then [`StartStaging::adopt`].
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
    registration: impl Into<ProcessStartRegistration>,
    observers: &[SessionId],
) -> Result<RegisteredProcessStart, RuntimeEffectControllerError> {
    let PreparedProcessStart {
        staging,
        registration,
    } = stage_process_start(stores, registration, observers).await?;
    let anchor = staging.anchor();
    let committed = stores
        .registry
        .commit_process_registration(registration, anchor)
        .await;
    staging.adopt(stores, committed).await
}

/// A process start staged as a store-local effect of the call that
/// declares it (ADR 0132 §5): admitted and staged as every start is, its
/// registration prepared, and nothing registered. Its [`Self::rows`]
/// register it in the transaction that records the call's outcome, under
/// the call's owner's epoch fence; what it staged under `Start(key)` its
/// guard carries onto the record the rows commit, or ends once the starter
/// settles without one.
#[derive(Clone, Debug)]
pub struct StagedProcessStart {
    /// The row the rows register, or the one a process already holds under
    /// the start's key.
    pub record: ProcessRecord,
    /// Whether committing the rows creates the row.
    pub disposition: crate::ProcessRegistrationOutcome,
    /// The rows that register the start; `None` when a process already
    /// holds its key, and so is the start.
    pub rows: Option<lash_durable::domain::ProcessStartRows>,
}

/// A prepared registration as a store-local start's rows carry it: what a
/// dialect applies inside the commit that records the start's outcome.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedRegistration {
    /// The registration, its trace anchor bound.
    pub registration: ProcessRegistration,
    /// The sessions that observe the process from its creation.
    pub observers: Vec<SessionId>,
    /// The process id the registrar minted.
    pub process_id: super::ProcessId,
    /// When the registration was prepared: the row's creation instant.
    pub prepared_at_ms: u64,
}

impl StagedRegistration {
    /// The record its rows register once they commit.
    ///
    /// # Errors
    ///
    /// A registration its admission refuses.
    pub fn record(&self) -> Result<ProcessRecord, crate::PluginError> {
        Ok(ProcessRecord::from_prepared_registration(
            super::prepare_process_registration(self.registration.clone())?,
            self.process_id.clone(),
            self.prepared_at_ms,
        ))
    }

    /// The rows a store-local start commits.
    ///
    /// # Errors
    ///
    /// A registration that does not encode.
    pub fn rows(&self) -> Result<lash_durable::domain::ProcessStartRows, crate::PluginError> {
        Ok(lash_durable::domain::ProcessStartRows {
            process: self.process_id.clone(),
            registration_json: serde_json::to_string(self).map_err(|error| {
                crate::PluginError::Session(format!("failed to encode a process start: {error}"))
            })?,
        })
    }

    /// The registration `rows` carry.
    ///
    /// # Errors
    ///
    /// Rows that do not decode.
    pub fn decode(
        rows: &lash_durable::domain::ProcessStartRows,
    ) -> Result<Self, crate::PluginError> {
        serde_json::from_str(&rows.registration_json).map_err(|error| {
            crate::PluginError::StoredDataCorrupt {
                record_kind: "process start rows".to_owned(),
                message: error.to_string(),
            }
        })
    }
}

/// Stage one process start as a store-local effect: [`stage_process_start`],
/// then the rows its call's outcome commits ([`StagedProcessStart`]).
///
/// A start whose key a process already holds is that process, with no rows.
/// A start whose `Start(key)` was fenced before this attempt, while no
/// process holds the key, is refused: the key's earlier attempt settled it,
/// and nothing would hold what this one names once its row committed.
///
/// # Errors
///
/// [`stage_process_start`]'s refusals, the fenced key's, and any store
/// failure.
pub async fn stage_store_local_start(
    stores: &ProcessStartStores<'_>,
    registration: impl Into<ProcessStartRegistration>,
    observers: &[SessionId],
) -> Result<StagedProcessStart, RuntimeEffectControllerError> {
    let PreparedProcessStart {
        staging,
        registration,
    } = stage_process_start(stores, registration, observers).await?;
    if registration.retained() {
        let record = stores
            .registry
            .get_process(registration.process_id())
            .await?
            .ok_or_else(|| crate::StoreError::PreparedProcessRegistrationStale {
                process_id: registration.process_id().clone(),
            })
            .map_err(crate::PluginError::from)?;
        staging.settle_trace(lash_trace::TraceCandidateOutcome::Reused);
        return Ok(StagedProcessStart {
            record,
            disposition: crate::ProcessRegistrationOutcome::Existing,
            rows: None,
        });
    }
    if staging.fenced() {
        let start_key = staging.start_key.clone();
        staging.abandon(stores).await?;
        return Err(RuntimeEffectControllerError::foreign(
            "process_start_key_settled",
            TurnFailureCause::Outcome,
            format!(
                "{}: start key `{start_key}` was settled by an earlier attempt that registered no process",
                stores.executor
            ),
        ));
    }
    let anchor = staging.anchor();
    let (registration, observers, process_id, _, prepared_at_ms) = registration.into_commit(anchor);
    // The registrar refuses a start whose starter or lifetime scope closed
    // (FIG-3607 R11) when its rows commit, and with it the commit of its
    // call's outcome. A closed scope never reopens, so the start is refused
    // here as the registrar would refuse it, and its call settles with the
    // typed refusal: a call whose commit was refused stages again on its
    // next attempt and ends here.
    for parent in registration.closing_scopes() {
        if stores
            .registry
            .get_parent_end_plan(&parent)
            .await?
            .is_some()
        {
            let start_key = registration.start_key.clone();
            staging.abandon(stores).await?;
            return Err(crate::PluginError::ParentEnded { start_key, parent }.into());
        }
    }
    let staged = StagedRegistration {
        registration,
        observers,
        process_id,
        prepared_at_ms,
    };
    let record = staged.record()?;
    let rows = staged.rows()?;
    staging.settle_trace(lash_trace::TraceCandidateOutcome::Selected);
    Ok(StagedProcessStart {
        record,
        disposition: crate::ProcessRegistrationOutcome::Created,
        rows: Some(rows),
    })
}

/// A process start staged and its registration prepared, before the
/// registrar's transaction that applies the registration.
pub struct PreparedProcessStart<'a> {
    /// What the start staged, adopted once the registration commits.
    pub staging: StartStaging<'a>,
    /// The registration to apply, its process id minted.
    pub registration: crate::PreparedProcessRegistration,
}

/// What one prepared start staged under `Start(key)`.
pub struct StartStaging<'a> {
    start_key: StartKey,
    claim: ReferrerClaim,
    definition: Option<StagedDefinition<'a>>,
    env: Option<StagedEnv>,
    engine: Option<StagedEngine<'a>>,
    submitted_env_ref: Option<ProcessExecutionEnvRef>,
    submitted_input: Arc<ProcessInput>,
    anchor: lash_trace::TraceAnchor,
    candidate: Option<Box<dyn lash_trace::TraceAdmissionCandidate>>,
}

/// Admit and stage one process start, and prepare its registration: every
/// step of a start before the transaction that registers it. Nothing here
/// writes a process row.
///
/// The host's live services (the session catalog and the session-turn
/// admission) are asked here, while no process
/// holds the key. A terminal refusal while none does ends `Start(key)`.
///
/// # Errors
///
/// A refusal for a start with no key or an executor missing a store the
/// start needs, the admission's refusal, and any store failure.
pub async fn stage_process_start<'a>(
    stores: &ProcessStartStores<'a>,
    registration: impl Into<ProcessStartRegistration>,
    observers: &[SessionId],
) -> Result<PreparedProcessStart<'a>, RuntimeEffectControllerError> {
    let registration = registration.into();
    let Some(start_key) = registration.start_key.clone() else {
        return Err(RuntimeEffectControllerError::foreign(
            "process_start_key_missing",
            TurnFailureCause::Outcome,
            "a journaled process start must carry its start key",
        ));
    };
    let claim = ReferrerClaim::guarded(ReferrerGuard::Start {
        start_key: start_key.clone(),
        starter: stores.starter.clone(),
    });
    let prepared = async {
        require_host_session_live(stores, &registration).await?;
        if let Some(admit) = stores.session_turn_admission {
            let fresh = stores
                .registry
                .get_process_by_start_key(&start_key)
                .await?
                .is_none();
            match admit(fresh, claim.clone()).await {
                // An earlier attempt settled the key: staging meets the same
                // fence and adopts what the key retains.
                Err(error) if error.ended_referrer() == Some(&claim.referrer()) => {}
                admitted => admitted?,
            }
        }
        stage(stores, &start_key, claim, registration, observers).await
    }
    .await;
    if let Err(error) = &prepared
        && error.is_terminal()
    {
        abandon_start(stores, start_key.clone()).await?;
        if let Some(ports) = stores.ports() {
            ports.nudge(&start_input_referrer(stores, &start_key)).await;
        }
    }
    prepared
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
    registration: &ProcessStartRegistration,
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

/// End `Start(key)` after a terminal refusal, unless a process already holds
/// the key: then the registered row is the start's outcome, and its guard
/// carries onto it.
///
/// A key is global, so another start may be staged under `Start(key)` while
/// this one is refused (FIG-4111). Its row can commit between the read below
/// and the end, and `Start(key)`'s end then carries nothing onto it. Either
/// that start meets the fence once its row commits and holds its own content
/// under `ProcessRecord` ([`StartStaging::adopt`]), or it looked before the
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
    ports
        .end(ArtifactReferrer::Start(start_key.clone()))
        .await?;
    // A start that is its own operation has no other end for its input
    // staging: no execution behind its starter ever settles.
    if is_start_operation(stores.starter, &start_key) {
        ports.end(start_input_referrer(stores, &start_key)).await?;
    }
    Ok(())
}

/// The journal of a start that runs as its own runtime operation: a local
/// start with no causal effect (ADR 0113 §3.3).
///
/// No execution journals under it, so it never settles on its own: the
/// start's staging claims are its lifecycle. Its registration carries them
/// onto its record, and its abandonment ends them.
///
/// # Errors
///
/// The key renders no admissible operation id.
pub fn start_operation_journal(
    start_key: &StartKey,
) -> Result<lash_sansio::EffectJournalIdentity, lash_sansio::EffectIdentityError> {
    crate::ExecutionScope::runtime_operation(crate::ProcessCommand::start_effect_id(Some(
        start_key,
    )))
    .journal_identity()
}

/// Whether `starter` is `start_key`'s own operation
/// ([`start_operation_journal`]).
#[must_use]
pub fn is_start_operation(
    starter: &lash_sansio::EffectJournalIdentity,
    start_key: &StartKey,
) -> bool {
    start_operation_journal(start_key).is_ok_and(|journal| journal == *starter)
}

async fn stage<'a>(
    stores: &ProcessStartStores<'a>,
    start_key: &StartKey,
    claim: ReferrerClaim,
    registration: ProcessStartRegistration,
    observers: &[SessionId],
) -> Result<PreparedProcessStart<'a>, RuntimeEffectControllerError> {
    let env = stage_env(stores, &claim, registration.env_ref.clone()).await?;
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
    let (definition, mut registration) =
        resolve_start_target(stores, &claim, registration, env_spec.as_ref()).await?;
    let engine = stage_engine(stores, &claim, &registration, env.as_ref()).await?;
    registration.engine_config = creation_config(stores, &registration, env_spec.as_ref())?;
    Box::pin(stage_input(stores, start_key, registration.input.as_ref())).await?;
    let submitted_env_ref = registration.env_ref.clone();
    let submitted_input = Arc::clone(&registration.input);
    let prepared = stores
        .registry
        .prepare_process_registration(registration, observers)
        .await?;
    let candidate = stores.tracing.map(|tracing| {
        tracing.scopes().propose(
            &lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::Process {
                process_id: prepared.process_id().clone(),
            }),
            prepared.trace().cause(),
        )
    });
    let anchor = candidate.as_ref().map_or_else(
        || prepared.trace().anchor().clone(),
        |candidate| candidate.anchor(),
    );
    Ok(PreparedProcessStart {
        staging: StartStaging {
            start_key: start_key.clone(),
            claim,
            definition,
            env,
            engine,
            submitted_env_ref,
            submitted_input,
            anchor,
            candidate,
        },
        registration: prepared,
    })
}

impl StartStaging<'_> {
    /// Whether `Start(key)` was fenced before this attempt staged: an
    /// earlier attempt settled the key, so nothing was staged under it.
    fn fenced(&self) -> bool {
        self.definition
            .as_ref()
            .is_some_and(|definition| !definition.staged)
            || self.env.as_ref().is_some_and(|env| !env.staged)
            || self.engine.as_ref().is_some_and(|engine| !engine.staged)
    }

    /// End the start's trace candidate with `outcome`, for a start whose
    /// row commits elsewhere.
    fn settle_trace(self, outcome: lash_trace::TraceCandidateOutcome) {
        if let Some(candidate) = self.candidate {
            candidate.settle(outcome);
        }
    }

    /// The trace anchor the registration commits with.
    #[must_use]
    pub fn anchor(&self) -> lash_trace::TraceAnchor {
        self.anchor.clone()
    }

    /// Settle the start once the transaction that applies its registration
    /// answered `committed`: hold what the row adopts under its record and
    /// nudge `Start(key)`'s guard, or, on a terminal refusal while no
    /// process holds the key, end `Start(key)`.
    ///
    /// # Errors
    ///
    /// `committed`'s refusal, and any store failure holding the row's
    /// content.
    pub async fn adopt(
        self,
        stores: &ProcessStartStores<'_>,
        committed: Result<crate::ProcessRegistrationReceipt, crate::PluginError>,
    ) -> Result<RegisteredProcessStart, RuntimeEffectControllerError> {
        let start_key = self.start_key.clone();
        match self.adopt_committed(stores, committed).await {
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

    /// Give the start up after the transaction that would have applied its
    /// registration refused it: `Start(key)` ends unless a process holds the
    /// key.
    ///
    /// # Errors
    ///
    /// Any store failure ending the referrer.
    pub async fn abandon(
        self,
        stores: &ProcessStartStores<'_>,
    ) -> Result<(), RuntimeEffectControllerError> {
        if let Some(candidate) = self.candidate {
            candidate.settle(lash_trace::TraceCandidateOutcome::Refused);
        }
        abandon_start(stores, self.start_key.clone()).await?;
        if let Some(ports) = stores.ports() {
            ports
                .nudge(&start_input_referrer(stores, &self.start_key))
                .await;
        }
        Ok(())
    }

    async fn adopt_committed(
        self,
        stores: &ProcessStartStores<'_>,
        committed: Result<crate::ProcessRegistrationReceipt, crate::PluginError>,
    ) -> Result<RegisteredProcessStart, RuntimeEffectControllerError> {
        let Self {
            start_key: _,
            claim,
            definition,
            env,
            engine,
            submitted_env_ref,
            submitted_input,
            anchor: _,
            candidate,
        } = self;
        if let Some(candidate) = candidate {
            candidate.settle(match &committed {
                Ok(receipt) if receipt.is_created() => lash_trace::TraceCandidateOutcome::Selected,
                Ok(_) => lash_trace::TraceCandidateOutcome::Reused,
                Err(_) => lash_trace::TraceCandidateOutcome::Refused,
            });
        }
        let registered = committed?;
        let disposition = registered.outcome;
        let created = disposition == crate::ProcessRegistrationOutcome::Created;
        let record = registered.record;
        let process_claim =
            ReferrerClaim::unguarded(ArtifactReferrer::ProcessRecord(record.id.clone()))
                .map_err(|error| crate::PluginError::Session(error.to_string()))?;
        let adopts_env = created || record.env_ref == submitted_env_ref;
        let adopts_engine = created || record.input == submitted_input;
        // A key is global, so another start's terminal refusal can end
        // `Start(key)` after this start staged there and before its row
        // committed (`abandon_start`, FIG-4111). The guard then carries
        // nothing onto the row: this start holds what it staged under
        // `ProcessRecord` itself.
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
        // StartInput holds the input before registration. Both this step and
        // its cleanup acquire the retained record before staging's attachment
        // edges end, so an abandoned caller leaves the handoff to the cleanup
        // relay.
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
    let claim = ReferrerClaim::guarded(ReferrerGuard::StartInput {
        start_key: start_key.clone(),
        starter: stores.starter.clone(),
    });
    let acquired = ports
        .attachments()
        .acquire_attachment_refs(&claim, &ids)
        .await;
    match acquired {
        Ok(()) => return Ok(()),
        Err(crate::StoreError::ArtifactReferrerEnded { referrer })
            if referrer == claim.referrer() => {}
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
            referrer: claim.referrer(),
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
    env_ref: Option<ProcessExecutionEnvRef>,
) -> Result<Option<StagedEnv>, RuntimeEffectControllerError> {
    let Some(env_ref) = env_ref else {
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
    let (ProcessInput::Engine { kind, .. }, Some(env_spec)) =
        (registration.input.as_ref(), env_spec)
    else {
        return Ok(None);
    };
    Ok(stores.engines.require(kind)?.creation_config(env_spec)?)
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
    let engines = stores.engines;
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

/// The registration a start's target resolves to: the input it states, or
/// the engine start its definition is ([`stage_definition`]). This is the
/// only place a start becomes a registration, so no registrar is ever handed
/// an unresolved definition.
async fn resolve_start_target<'a>(
    stores: &ProcessStartStores<'a>,
    claim: &ReferrerClaim,
    registration: ProcessStartRegistration,
    env_spec: Option<&ProcessExecutionEnvSpec>,
) -> Result<(Option<StagedDefinition<'a>>, ProcessRegistration), RuntimeEffectControllerError> {
    match registration.input.as_ref() {
        ProcessStartTarget::Input(input) => {
            let identity = match input {
                ProcessInput::Engine { kind, payload } => {
                    Some(stores.engines.admit(kind, payload, env_spec).await?)
                }
                _ => None,
            };
            let input = Arc::new(input.clone());
            let declared_label = registration.identity.label.clone();
            let registration = match identity {
                Some(identity) => registration
                    .with_admitted_identity(identity)
                    .with_host_facing_label(declared_label),
                None => registration,
            };
            Ok((None, registration.with_input(input)))
        }
        ProcessStartTarget::Definition {
            definition_id,
            signature_claim,
            args,
        } => {
            let (staged, registration) = stage_definition(
                stores,
                claim,
                &registration,
                definition_id,
                signature_claim.as_ref(),
                args,
                env_spec,
            )
            .await?;
            Ok((Some(staged), registration))
        }
    }
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
    stores: &ProcessStartStores<'a>,
    claim: &ReferrerClaim,
    registration: &ProcessStartRegistration,
    definition_id: &super::ProcessDefinitionId,
    signature_claim: Option<&super::ProcessSignature>,
    args: &serde_json::Map<String, serde_json::Value>,
    env_spec: Option<&ProcessExecutionEnvSpec>,
) -> Result<(StagedDefinition<'a>, ProcessRegistration), RuntimeEffectControllerError> {
    let engines = stores.engines;
    let Some(ports) = stores.ports() else {
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
    let mut identity = engines
        .admit(&kind, &payload, env_spec)
        .await?
        .into_identity();
    identity.definition_id = Some(resolved.id().clone());
    let id = resolved.id().clone();
    let registration = registration
        .clone()
        .with_input(Arc::new(ProcessInput::Engine { kind, payload }))
        .with_admitted_identity(super::AdmittedProcessIdentity::admitted(identity))
        .with_host_facing_label(declared_label);
    Ok((
        StagedDefinition {
            engines,
            ports,
            id,
            staged,
        },
        registration,
    ))
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
