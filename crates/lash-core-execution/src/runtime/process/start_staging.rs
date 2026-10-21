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

use lash_execution_contracts::process::StagedRegistration;
use std::collections::BTreeSet;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

use super::HoldEnvError;
use super::ProcessEngineRegistry;
use super::ProcessEnvPorts;
use super::ProcessEnvRef;
use super::ProcessEnvSelection;
use super::SessionId;
use super::start_phases::{StartPhaseClock, mark, timed};
use crate::ArtifactCleanup;
use crate::ArtifactName;
use crate::ArtifactReferrer;
use crate::ArtifactStoreId;
use crate::ProcessEnv;
use crate::ReferrerClaim;
use crate::ReferrerGuard;
use crate::RuntimeEffectControllerError;
use crate::StartKey;
use crate::TurnFailureCause;
use crate::runtime::Clock;
use crate::store::ArtifactCleanupLedger;
use lash_execution_contracts::module_artifacts::ModuleArtifactStore;
use lash_execution_contracts::process::ProcessEnvStore;
use lash_execution_contracts::process::ProcessInput;
use lash_execution_contracts::process::ProcessRecord;
use lash_execution_contracts::process::ProcessRegistration;
use lash_execution_contracts::process::ProcessRegistry;
use lash_execution_contracts::process::ProcessStartRegistration;
use lash_execution_contracts::process::ProcessStartTarget;
use lash_execution_contracts::process::StoreRealization;
use lash_execution_contracts::process::artifact_referrer_ended;
use lash_trace::ProcessStartPhase as Phase;

/// The artifact stores a referrer acquires through, and the cleanup ledger
/// its guards arm in (ADR 0113 §2.1, §2.4): what a process start and a
/// definition publication need to hold every artifact a
/// [`ProcessEngine::start_artifacts`](lash_execution_contracts::process::ProcessEngine::start_artifacts)
/// name points at, whichever store holds it.
#[derive(Clone)]
pub struct ArtifactReferrerPorts {
    modules: Arc<dyn ModuleArtifactStore>,
    env: Arc<dyn ProcessEnvStore>,
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

#[path = "start_referrers.rs"]
mod referrers;
use referrers::held_or_ended;

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
    /// Times the start's phases for its producer span; `None` when nothing
    /// observes the start.
    pub phases: Option<Arc<StartPhaseClock>>,
}

/// The stores one process start writes through.
pub struct ProcessStartStores<'a> {
    pub tracing: Option<&'a crate::trace::TraceRuntime>,
    pub registry: &'a dyn ProcessRegistry,
    pub env_store: Option<&'a Arc<dyn ProcessEnvStore>>,
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
    pub starter: &'a lash_protocol_types::EffectJournalIdentity,
    /// Times the start's phases; `None` for a start nothing times.
    pub phases: Option<&'a StartPhaseClock>,
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
    pub disposition: lash_execution_contracts::process::ProcessRegistrationOutcome,
    /// The execution environment the start's registration names once staged.
    pub env_ref: Option<ProcessEnvRef>,
}

impl RegisteredProcessStart {
    /// The registry's verdict: a coalesced start is reported as a replay
    /// rather than a fresh start (FIG-3070).
    pub fn realization(&self) -> StoreRealization {
        StoreRealization::from_wrote(
            self.disposition
                == lash_execution_contracts::process::ProcessRegistrationOutcome::Created,
        )
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
        let registration = registration
            .into()
            .with_trace_class(self.record.trace_class);
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
            ProcessStartTarget::Definition { .. } | ProcessStartTarget::Inline { .. } => {
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
    let committed = stores
        .registry
        .commit_process_registration(registration)
        .await;
    mark(stores.phases, Phase::Commit);
    let adopted = staging.adopt(stores, committed).await;
    mark(stores.phases, Phase::Adopt);
    adopted
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
    pub disposition: lash_execution_contracts::process::ProcessRegistrationOutcome,
    /// The rows that register the start; `None` when a process already
    /// holds its key, and so is the start.
    pub rows: Option<lash_durable::domain::ProcessStartRows>,
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
            .map_err(lash_execution_contracts::plugin_error::PluginError::from)?;
        return Ok(StagedProcessStart {
            record,
            disposition: lash_execution_contracts::process::ProcessRegistrationOutcome::Existing,
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
    let execution = registration.execution.clone();
    let initial_resolutions = registration.initial_resolutions.clone();
    let (registration, observers, process_id, _, prepared_at_ms) = registration.into_commit();
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
            return Err(
                lash_execution_contracts::plugin_error::PluginError::ParentEnded {
                    start_key,
                    parent,
                }
                .into(),
            );
        }
    }
    let staged = StagedRegistration {
        execution,
        initial_resolutions,
        registration,
        observers,
        process_id,
        prepared_at_ms,
    };
    let record = staged.record()?;
    let rows = staged.rows()?;
    Ok(StagedProcessStart {
        record,
        disposition: lash_execution_contracts::process::ProcessRegistrationOutcome::Created,
        rows: Some(rows),
    })
}

/// A process start staged and its registration prepared, before the
/// registrar's transaction that applies the registration.
pub struct PreparedProcessStart<'a> {
    /// What the start staged, adopted once the registration commits.
    pub staging: StartStaging<'a>,
    /// The registration to apply, its process id minted.
    pub registration: lash_execution_contracts::process::PreparedProcessRegistration,
}

/// What one prepared start staged under `Start(key)`.
pub struct StartStaging<'a> {
    start_key: StartKey,
    claim: ReferrerClaim,
    definition: Option<StagedDefinition<'a>>,
    env: Option<StagedEnv>,
    engine: Option<StagedEngine<'a>>,
    submitted_env_ref: Option<ProcessEnvRef>,
    submitted_input: Arc<ProcessInput>,
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
    let registration = registration
        .into()
        .with_trace_nodes(stores.tracing.is_some_and(|tracing| {
            tracing
                .detail()
                .get(lash_trace::TraceSpanKind::Workflow)
                .is_extended()
        }));
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
    mark(stores.phases, Phase::Dispatch);
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
        mark(stores.phases, Phase::Admission);
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
    let lash_execution_contracts::process::LifetimeDecision::Until {
        scope: super::model::ScopeId::Session(session_id),
        grant: lash_execution_contracts::process::ScopeGrant::HostSessionLookup,
    } = &registration.lifetime
    else {
        return Ok(());
    };
    let live = match stores.session_catalog {
        Some(catalog) => lash_execution_contracts::deployment::session_is_live(catalog, session_id)
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
) -> Result<lash_protocol_types::EffectJournalIdentity, lash_protocol_types::EffectIdentityError> {
    crate::ExecutionScope::runtime_operation(crate::ProcessCommand::start_effect_id(Some(
        start_key,
    )))
    .journal_identity()
}

/// Whether `starter` is `start_key`'s own operation
/// ([`start_operation_journal`]).
#[must_use]
pub fn is_start_operation(
    starter: &lash_protocol_types::EffectJournalIdentity,
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
    let mut submitted_env_ref = registration.input.launch().map(|launch| launch.env.clone());
    let mut env = stage_env(stores, &claim, submitted_env_ref.clone()).await?;
    mark(stores.phases, Phase::Environment);
    let (definition, mut registration) = resolve_start_target(
        stores,
        start_key,
        &claim,
        env.as_ref().is_none_or(|env| env.staged),
        registration,
    )
    .await?;
    registration.trace_class = match registration.input.as_ref() {
        ProcessInput::Engine { kind, .. } => stores.engines.require(kind)?.trace_class(),
        ProcessInput::SessionTurn { .. } => {
            lash_execution_contracts::process::ProcessTraceClass::Internal
        }
    };
    registration.trace_nodes &=
        registration.trace_class == lash_execution_contracts::process::ProcessTraceClass::Workflow;
    check_sealed_settings(stores, &registration)?;
    mark(stores.phases, Phase::Admit);
    let loaded = load_staged_env(stores, env.as_ref()).await?;
    submitted_env_ref = env
        .as_ref()
        .map(|env| env.env_ref.clone())
        .or(submitted_env_ref);
    mark(stores.phases, Phase::Environment);
    registration.requires = loaded
        .as_ref()
        .map(|env| {
            crate::plugin::ProcessComponentDeclaration::env_requirements(env)
                .into_iter()
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let inspection = inspect_start(stores.engines, registration.input.as_ref()).await?;
    let mut execution_requirements = loaded
        .as_ref()
        .map(|env| env.execution().requirements.clone())
        .unwrap_or_default();
    if let Some(inspection) = &inspection {
        let env =
            loaded.as_ref().ok_or_else(|| {
                package_start_error(
            lash_execution_contracts::process::linking::EnginePortError::Refused(
                lash_execution_contracts::process::linking::AdmissionRefusal::InvalidDocument {
                    message: "a linked engine start requires an execution environment".into(),
                }))
            })?;
        execution_requirements.extend(
            &stores
                .engines
                .check_links(&Arc::new(env.clone()), inspection)
                .map_err(package_start_error)?,
        );
        if let Some(launch) = registration.input.launch() {
            execution_requirements.extend(
                &crate::runtime::actor::resolutions::validate_seeds(
                    inspection,
                    env.execution(),
                    &launch.initial_resolutions,
                    |grant, name, definition, route, registry| {
                        stores.engines.components().validate_seed_route(
                            &Arc::new(env.clone()),
                            grant,
                            name,
                            definition,
                            route,
                            registry,
                        )
                    },
                )
                .map_err(package_start_error)?,
            );
        }
        registration
            .requires
            .extend(execution_requirements.tokens());
        registration.requires.sort();
        registration.requires.dedup();
    }
    let engine = stage_engine(
        stores,
        &claim,
        &registration,
        env.as_ref(),
        definition.as_ref(),
    )
    .await?;
    mark(stores.phases, Phase::Artifacts);
    // The engine's artifacts are held: its effects are read from them.

    mark(stores.phases, Phase::LinkCheck);
    Box::pin(stage_input(stores, start_key, registration.input.as_ref())).await?;
    mark(stores.phases, Phase::Input);
    let submitted_input = Arc::clone(&registration.input);
    let mut prepared = stores
        .registry
        .prepare_process_registration(registration, observers)
        .await?;
    if let Some(inspection) = inspection {
        let execution = lash_core_store::resolutions::ExecutionId::process(prepared.process_id());
        let seeds = submitted_input
            .launch()
            .into_iter()
            .flat_map(|launch| &launch.initial_resolutions)
            .map(|seed| {
                let requested = match inspection.operations.get(&seed.use_) {
                    Some(lash_execution_contracts::process::linking::OperationNeed::Deferred {
                        name,
                        ..
                    }) => name.clone(),
                    _ => unreachable!("initial pins were validated against selected reach"),
                };
                lash_core_store::resolutions::ResolutionRecord {
                    key: lash_core_store::resolutions::ResolutionKey {
                        execution: execution.clone(),
                        use_: seed.use_,
                    },
                    requested,
                    entry: seed.entry.clone(),
                }
            })
            .collect();
        let membership = crate::runtime::actor::resolutions::registration(
            execution,
            &inspection,
            ArtifactReferrer::ProcessRecord(prepared.process_id().clone()),
            execution_requirements,
        );
        prepared = prepared.with_execution(membership, seeds);
    }
    mark(stores.phases, Phase::Prepare);
    Ok(PreparedProcessStart {
        staging: StartStaging {
            start_key: start_key.clone(),
            claim,
            definition,
            env,
            engine,
            submitted_env_ref,
            submitted_input,
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
        committed: Result<
            lash_execution_contracts::process::ProcessRegistrationReceipt,
            lash_execution_contracts::plugin_error::PluginError,
        >,
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
        committed: Result<
            lash_execution_contracts::process::ProcessRegistrationReceipt,
            lash_execution_contracts::plugin_error::PluginError,
        >,
    ) -> Result<RegisteredProcessStart, RuntimeEffectControllerError> {
        let Self {
            start_key: _,
            claim,
            definition,
            env,
            engine,
            submitted_env_ref,
            submitted_input,
        } = self;
        let registered = committed?;
        let disposition = registered.outcome;
        let created =
            disposition == lash_execution_contracts::process::ProcessRegistrationOutcome::Created;
        let record = registered.record;
        let process_claim = ReferrerClaim::unguarded(ArtifactReferrer::ProcessRecord(
            record.id.clone(),
        ))
        .map_err(|error| {
            lash_execution_contracts::plugin_error::PluginError::Session(error.to_string())
        })?;
        let adopts_env = created || record.env_ref() == submitted_env_ref.as_ref();
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
                == lash_execution_contracts::process::DefinitionAcquisition::Ended
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
        return Ok(acquired == lash_execution_contracts::process::DefinitionAcquisition::Ended);
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

/// Hold the start's environment under `claim` through the one hold path
/// (D10): `true` when the claim holds it, `false` when the claim met its
/// fence. An environment that is not published refuses the start.
async fn acquire_env(
    env_store: &dyn ProcessEnvStore,
    claim: &ReferrerClaim,
    env: &StagedEnv,
) -> Result<bool, RuntimeEffectControllerError> {
    let held = super::hold_process_env(
        &ProcessEnvPorts::published_only(env_store),
        claim,
        &ProcessEnvSelection::Published(env.env_ref.clone()),
    )
    .await;
    let acquired = match held {
        Ok(_) => Ok(()),
        Err(HoldEnvError::Fault(error)) => Err(error),
        Err(HoldEnvError::Refused(refusal)) => {
            return Err(
                lash_execution_contracts::plugin_error::PluginError::ProcessEnvRefused { refusal }
                    .into(),
            );
        }
    };
    Ok(held_or_ended(claim, acquired)?.is_none())
}

async fn stage_env(
    stores: &ProcessStartStores<'_>,
    claim: &ReferrerClaim,
    env_ref: Option<ProcessEnvRef>,
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

/// The staged environment, read back verified: it is held. A process under
/// it requires its components' config tokens (§6.2).
async fn load_staged_env(
    stores: &ProcessStartStores<'_>,
    env: Option<&StagedEnv>,
) -> Result<Option<ProcessEnv>, RuntimeEffectControllerError> {
    let (Some(env), Some(env_store)) = (env, stores.env_store) else {
        return Ok(None);
    };
    let loaded =
        lash_execution_contracts::process::load_process_env(env_store.as_ref(), &env.env_ref)
            .await
            .map_err(|error| {
                crate::runtime::effect::executor::unresolved_execution_env(
                    stores.executor,
                    &env.env_ref,
                    error,
                )
            })?;
    Ok(Some(loaded))
}

fn package_start_error(
    error: lash_execution_contracts::process::linking::EnginePortError,
) -> RuntimeEffectControllerError {
    lash_execution_contracts::plugin_error::PluginError::Package(Box::new(error)).into()
}

pub(crate) async fn inspect_start(
    engines: &ProcessEngineRegistry,
    input: &ProcessInput,
) -> Result<
    Option<lash_execution_contracts::process::linking::LinkInspection>,
    RuntimeEffectControllerError,
> {
    let ProcessInput::Engine { kind, payload, .. } = input else {
        return Ok(None);
    };
    let Some(provider) = engines.document_provider(kind) else {
        return Ok(None);
    };
    match provider.document_ref(payload).await? {
        lash_execution_contracts::process::ProcessDocumentRefRead::Named(start) => engines
            .inspect_link(kind, &start)
            .await
            .map(Some)
            .map_err(package_start_error),
        lash_execution_contracts::process::ProcessDocumentRefRead::ArtifactMissing { artifact } => {
            Err(package_start_error(
                lash_execution_contracts::process::linking::EnginePortError::Missing { artifact },
            ))
        }
    }
}

/// The engine of an engine start checks the settings its starter sealed,
/// defensively, before the row is registered (D4). Registration copies the
/// settings; it never chooses them.
fn check_sealed_settings(
    stores: &ProcessStartStores<'_>,
    registration: &ProcessRegistration,
) -> Result<(), RuntimeEffectControllerError> {
    let ProcessInput::Engine {
        kind,
        payload,
        launch,
    } = registration.input.as_ref()
    else {
        return Ok(());
    };
    Ok(stores
        .engines
        .check_settings(kind, payload, launch.settings.as_ref())?)
}

async fn stage_engine<'a>(
    stores: &ProcessStartStores<'a>,
    claim: &ReferrerClaim,
    registration: &ProcessRegistration,
    env: Option<&StagedEnv>,
    definition: Option<&StagedDefinition<'_>>,
) -> Result<Option<StagedEngine<'a>>, RuntimeEffectControllerError> {
    let ProcessInput::Engine { kind, payload, .. } = registration.input.as_ref() else {
        return Ok(None);
    };
    let engines = stores.engines;
    // A checked definition's manifest already holds these names under this
    // same claim. Keep only artifacts the actual start payload adds (for
    // example through its arguments); adoption carries the definition's
    // complete closure independently of this remainder.
    let names: Vec<_> = engines
        .require(kind)?
        .start_artifacts(payload)?
        .into_iter()
        .filter(|name| {
            !definition
                .is_some_and(|definition| definition.staged && definition.artifacts.contains(name))
        })
        .collect();
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
/// the engine start its definition is ([`stage_definition`]), or the one
/// the function it carries inline is once realized ([`stage_inline`]). This
/// is the only place a start becomes a registration, so no registrar is ever
/// handed an unresolved definition. `unfenced` is false when the start's
/// environment met `Start(key)`'s fence: nothing more is staged there.
async fn resolve_start_target<'a>(
    stores: &ProcessStartStores<'a>,
    start_key: &StartKey,
    claim: &ReferrerClaim,
    unfenced: bool,
    registration: ProcessStartRegistration,
) -> Result<(Option<StagedDefinition<'a>>, ProcessRegistration), RuntimeEffectControllerError> {
    match registration.input.as_ref() {
        ProcessStartTarget::Input(input) => {
            let identity = match input {
                ProcessInput::Engine { kind, payload, .. } => {
                    mark(stores.phases, Phase::Definition);
                    Some(stores.engines.admit(kind, payload).await?)
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
            launch,
        } => {
            let (staged, registration) = stage_definition(
                stores,
                claim,
                &registration,
                definition_id,
                signature_claim.as_ref(),
                args,
                launch,
            )
            .await?;
            Ok((Some(staged), registration))
        }
        ProcessStartTarget::Inline {
            engine_kind,
            request,
            args,
            launch,
        } => {
            let (staged, resolved) =
                stage_inline(stores, start_key, claim, unfenced, engine_kind, request).await?;
            let registration =
                definition_registration(stores, &registration, &resolved, args, launch).await?;
            Ok((
                Some(StagedDefinition {
                    engines: stores.engines,
                    ports: definition_ports(stores, resolved.id())?,
                    id: resolved.id().clone(),
                    artifacts: resolved.draft.artifacts().iter().cloned().collect(),
                    staged,
                }),
                registration,
            ))
        }
    }
}

/// The artifact ports a start of `definition_id` holds it through.
fn definition_ports<'a>(
    stores: &ProcessStartStores<'a>,
    definition_id: &super::ProcessDefinitionId,
) -> Result<&'a ArtifactReferrerPorts, RuntimeEffectControllerError> {
    stores.ports().ok_or_else(|| {
        RuntimeEffectControllerError::foreign(
            "process_definition_store_unavailable",
            TurnFailureCause::Outcome,
            format!(
                "admitted {} starts definition `{definition_id}` but the executor has no \
                 definition store to hold it",
                stores.executor
            ),
        )
    })
}

/// Realize a function a start carries inline (FIG-6382), in the order its
/// crash cuts are proved in: its engine checks it, publishes its document
/// and every declaration the store lacks under `Start(key)` through the one
/// document writer, then its descriptor joins them under the same claim. The
/// definition it answers is then started as [`stage_definition`] starts a
/// stored one, and the row adopts the whole closure. Answers whether
/// `Start(key)` holds it.
///
/// A key a process already holds stages nothing: that row is the start's
/// outcome, and its record holds its closure or `Start(key)`'s guard carries
/// it there; an exact retry is answered that process, another function under
/// a host's key is refused here as a conflict, and other arguments or another
/// launch are the registrar's conflict. A fenced `Start(key)` adds no late edge either:
/// the definition is read, and the row's record holds it once registered.
async fn stage_inline(
    stores: &ProcessStartStores<'_>,
    start_key: &StartKey,
    claim: &ReferrerClaim,
    unfenced: bool,
    engine_kind: &str,
    request: &lash_execution_contracts::process::InlineRequest,
) -> Result<(bool, super::ResolvedProcessDefinition), RuntimeEffectControllerError> {
    let engines = stores.engines;
    let starts = engines.inline_starts(engine_kind)?;
    let resolved = |inline: super::InlineDefinition| super::ResolvedProcessDefinition {
        definition: lash_execution_contracts::process::ProcessDefinition::new(
            inline.draft.id(),
            inline.signature,
        ),
        draft: inline.draft,
    };
    if let Some(retained) = stores.registry.get_process_by_start_key(start_key).await? {
        let inline = starts.inspect(request).await.map_err(PluginError::from)?;
        // Another function under a key that fences content is the
        // registrar's conflict, decided before anything of it is staged:
        // its closure is held by nothing, and is not acquired here.
        if start_key.fences_content()
            && retained.identity.definition_id.as_ref() != Some(&inline.draft.id())
        {
            return Err(
                lash_execution_contracts::plugin_error::PluginError::StartKeyConflict {
                    start_key: start_key.clone(),
                }
                .into(),
            );
        }
        return Ok((false, resolved(inline)));
    }
    let inline = if unfenced {
        match starts.realize(claim, request).await {
            Ok(inline) => Some(inline),
            Err(lash_execution_contracts::process::linking::EnginePortError::Fenced) => None,
            Err(error) => return Err(PluginError::from(error).into()),
        }
    } else {
        None
    };
    if let Some(inline) = inline {
        let ports = definition_ports(stores, &inline.draft.id())?;
        match ports
            .publish_definition(engines, claim, &inline.draft)
            .await
        {
            Ok(_) => return Ok((true, resolved(inline))),
            Err(error) if super::artifact_referrer_ended(&error) == Some(&claim.referrer()) => {}
            Err(error) => return Err(error.into()),
        }
    }
    // `Start(key)` ended before this attempt held the closure: an earlier
    // attempt settled the key. Only a definition something else holds is
    // started.
    let inline = starts.inspect(request).await.map_err(PluginError::from)?;
    let id = inline.draft.id();
    if definition_ports(stores, &id)?
        .read_definition(engines, &id)
        .await?
        .is_none()
    {
        return Err(definition_missing(&id));
    }
    Ok((false, resolved(inline)))
}

fn definition_missing(definition_id: &super::ProcessDefinitionId) -> RuntimeEffectControllerError {
    lash_execution_contracts::plugin_error::PluginError::Runtime(crate::RuntimeError::new(
        crate::RuntimeErrorCode::DefinitionMissing,
        format!("process definition `{definition_id}` is not stored: no referrer holds it"),
    ))
    .into()
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
    launch: &super::EngineLaunch,
) -> Result<(StagedDefinition<'a>, ProcessRegistration), RuntimeEffectControllerError> {
    let engines = stores.engines;
    let ports = definition_ports(stores, definition_id)?;
    let (resolved, staged) = match ports
        .acquire_definition_timed(engines, claim, definition_id, stores.phases)
        .await?
    {
        lash_execution_contracts::process::DefinitionAcquisition::Held(resolved) => {
            (resolved, true)
        }
        lash_execution_contracts::process::DefinitionAcquisition::Ended => {
            let Some(resolved) = ports
                .resolve_stored_definition(engines, definition_id, stores.phases)
                .await?
            else {
                return Err(definition_missing(definition_id));
            };
            (resolved, false)
        }
    };
    if let Some(signature) = signature_claim {
        timed(
            stores.phases,
            Phase::DefinitionVerify,
            engines.verify_definition_claim(
                &resolved.draft,
                &lash_execution_contracts::process::ProcessDefinition::new(
                    definition_id.clone(),
                    signature.clone(),
                ),
            ),
        )
        .await
        .map_err(lash_execution_contracts::plugin_error::PluginError::from)?;
    }
    let registration =
        definition_registration(stores, registration, &resolved, args, launch).await?;
    Ok((
        StagedDefinition {
            engines,
            ports,
            id: resolved.id().clone(),
            artifacts: resolved.draft.artifacts().iter().cloned().collect(),
            staged,
        },
        registration,
    ))
}

/// The engine start a start of `resolved` with `args` registers, admitted
/// exactly as every engine start is: the registration carries that engine
/// input, and its identity names the definition id.
async fn definition_registration(
    stores: &ProcessStartStores<'_>,
    registration: &ProcessStartRegistration,
    resolved: &super::ResolvedProcessDefinition,
    args: &serde_json::Map<String, serde_json::Value>,
    launch: &super::EngineLaunch,
) -> Result<ProcessRegistration, RuntimeEffectControllerError> {
    let kind = resolved.draft.engine_kind().as_str().to_owned();
    let payload = resolved.start_payload(args)?;
    let declared_label = registration.identity.label.clone();
    mark(stores.phases, Phase::Definition);
    let mut identity = stores.engines.admit(&kind, &payload).await?.into_identity();
    identity.definition_id = Some(resolved.id().clone());
    Ok(registration
        .clone()
        .with_input(Arc::new(ProcessInput::Engine {
            kind,
            payload,
            launch: launch.clone(),
        }))
        .with_admitted_identity(
            lash_execution_contracts::process::engine::admitted_process_identity(identity),
        )
        .with_host_facing_label(declared_label))
}

struct StagedDefinition<'a> {
    engines: &'a ProcessEngineRegistry,
    ports: &'a ArtifactReferrerPorts,
    id: super::ProcessDefinitionId,
    /// The checked manifest acquired under this start's claim.
    artifacts: BTreeSet<ArtifactName>,
    /// Whether `Start(key)` holds the closure: `false` when it was already
    /// fenced.
    staged: bool,
}

struct StagedEnv {
    env_ref: ProcessEnvRef,
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
