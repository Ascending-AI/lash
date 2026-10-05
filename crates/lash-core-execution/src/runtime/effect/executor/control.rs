use crate::SessionId;
use crate::TurnId;
pub use lash_core_store::await_event_identity::*;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{AdmittedScope, RuntimeError, RuntimeErrorCode};

use super::super::envelope::{RuntimeEffectEnvelope, RuntimeEffectOutcome};
use super::TurnControlBinding;
use super::await_event_support::await_event_scope_not_retirable;
use super::{RuntimeEffectControllerError, RuntimeEffectLocalExecutor, TurnCancelWait};

mod progress;
pub use progress::{BoundaryReason, SegmentProgress};

use lash_core_effect::retirement;
pub mod scope;
pub mod task;
pub use lash_core_effect::AwaitEventResolver;
pub use lash_core_effect::CompletionKeyPreparation;
pub use retirement::*;
pub use scope::facade_ops;
pub use scope::*;
pub use task::*;

/// An engine's verdict on one effect journal (ADR 0113 §2.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalReplay {
    /// The journal may still replay or append.
    MayReplay,
    /// Nothing will replay or append to the journal again.
    Settled,
}

/// Backend-level factory for scoped effect controllers.
#[async_trait::async_trait]
pub trait EffectHost: AwaitEventResolver {
    /// Stable identity of the physical authority that owns this host's reserved
    /// turn-control promises. Implementors must preserve it across client or
    /// handler recreation for as long as issued keys remain recoverable.
    fn turn_control_binding_id(&self) -> String;

    /// Release a terminal run's wait-index rows after the scope-close sink
    /// records its end. `committed_turn` is the physical turn whose commit
    /// ended the run, when one did: that commit still owes its turn's
    /// terminal to every waiter, so it is never released as cancelled
    /// (FIG-4025). Hosts without a per-session wait index owe no work.
    async fn retire_closed_run_waits(
        &self,
        _session_id: &SessionId,
        _run: &TurnId,
        _committed_turn: Option<&TurnId>,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    /// List the registered, unresolved await-event keys owned by `session_id`.
    ///
    /// This is a deployment-administrative snapshot, not a replay-sensitive
    /// per-run observation. A key may resolve concurrently after it is
    /// returned; callers must handle the existing first-writer-wins
    /// [`ResolveOutcome`] when they act on it. Possession of a returned key is
    /// resolution authority, so hosts must expose this read only to callers
    /// authorized to resolve that session's waits.
    ///
    /// Hosts that cannot enumerate their registry fail explicitly rather than
    /// reporting an empty session.
    async fn list_outstanding_await_event_keys(
        &self,
        _session_id: &SessionId,
    ) -> Result<Vec<AwaitEventKey>, RuntimeError> {
        Err(RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventUnsupported,
            "this effect host does not support listing outstanding await-event keys",
        ))
    }

    /// Project the terminal attachment owned by this same effect deployment.
    /// A host with a dedicated attach transport overrides this projection;
    /// otherwise a caller attaches through the run's keyed promises.
    fn turn_attach(&self) -> Option<Arc<dyn crate::TurnAttach>> {
        None
    }
    fn scoped<'run>(
        &'run self,
        admitted: AdmittedScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError>;

    fn scoped_static(
        &self,
        _admitted: AdmittedScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        Ok(None)
    }

    /// Route a process segment's handler controller through this host's layers.
    /// A turn handler's controller is the embedder's to route.
    fn route_handler_child_controller<'run>(
        &self,
        controller: ScopedEffectController<'run>,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        Ok(controller)
    }

    /// Projects this host to the resolver that owns its await-event registry.
    fn await_event_resolver(&self) -> &dyn AwaitEventResolver;

    async fn turn_control_binding<'a>(
        &'a self,
        scoped: &'a ScopedEffectController<'_>,
    ) -> Result<TurnControlBinding<'a>, RuntimeError> {
        let binding_id = super::turn_control_binding_id_for_scope(
            &self.turn_control_binding_id(),
            scoped.execution_scope(),
        )?;
        let resolver = scoped.controller();
        let Some(controller_authority_id) = resolver.await_event_authority_binding_id() else {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::InvalidTurnCancelRequest,
                "durable turn-control controller does not identify its await-event authority",
            ));
        };
        let controller_binding_id = super::turn_control_binding_id_for_scope(
            &controller_authority_id,
            scoped.execution_scope(),
        )?;
        if controller_binding_id != binding_id {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::InvalidTurnCancelRequest,
                format!(
                    "turn-control host authority `{binding_id}` does not match controller authority `{controller_binding_id}`"
                ),
            ));
        }
        Ok(TurnControlBinding::run_scoped(
            binding_id,
            resolver,
            self.turn_attach(),
        ))
    }

    /// Retire durable effect history after its owning lifecycle is no longer
    /// executable.
    async fn retire_effect_journal(
        &self,
        _retirement: EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        Err(RuntimeError::new(
            crate::RuntimeErrorCode::EffectJournalRetirementUnsupported,
            "this effect host does not implement effect-journal retirement",
        ))
    }

    /// Whether `journal` may still replay or append (ADR 0113 §2.5). `Settled`
    /// is the engine's promise that nothing will replay or append to the
    /// journal again; the artifact-cleanup executor severs nothing an
    /// execution referrer holds, and nothing a gate protects, until it is.
    /// A wait retirement alone never proves it.
    async fn journal_replay(
        &self,
        journal: &lash_sansio::EffectJournalIdentity,
    ) -> Result<JournalReplay, RuntimeError>;

    /// Lift the scope-retirement fence of `scope` because its owner is being
    /// registered again: a pruned process id that a host re-registers starts
    /// its new incarnation unfenced, with the empty journal the prune left
    /// (ADR 0049). The fence row alone is removed; nothing is re-created.
    /// Session-bearing scopes are refused with
    /// `await_event_scope_not_retirable`, exactly as the scope lever refuses
    /// to retire them. A host that never fences (it does not implement
    /// scope-exact retirement) has nothing to lift and answers `Ok`.
    async fn reinstate_effect_scope(&self, scope: &ExecutionScope) -> Result<(), RuntimeError> {
        if scope.session_id().is_some() {
            return Err(await_event_scope_not_retirable(scope));
        }
        Ok(())
    }

    /// Bind the process registry that owns the process-scope fence
    /// (ADR 0049). Called by [`ProcessRegistrar::bind_effect_host`]
    /// (crate::ProcessRegistrar::bind_effect_host); idempotent.
    ///
    /// A host whose journal and registry share one backend is wired to the
    /// registry by that backend's location, not by this binding. A host
    /// whose fence is a cache (the Restate durable-wait index) keeps
    /// [`ProcessRegistryBinding::registrations`]
    /// (crate::ProcessRegistryBinding::registrations) and treats a fence
    /// on a registered process scope as stale. A host that never fences
    /// ignores the binding.
    fn bind_process_registry(&self, _binding: crate::ProcessRegistryBinding) {}

    /// Register one durable session catalog as a lifetime participant in a
    /// non-session scope. The owner serializes this with irreversible scope
    /// retirement; an existing retirement fence refuses registration.
    async fn register_turn_cancel_closure_participant(
        &self,
        _participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        if scope.session_id().is_some() {
            return Ok(());
        }
        Err(RuntimeError::new(
            RuntimeErrorCode::EffectJournalRetirementUnsupported,
            "this effect host does not implement cancellation-closure lifecycle participation",
        ))
    }

    /// Release a catalog's scope participant after that catalog has durably
    /// fenced new authorizations and proved that no authorization remains.
    async fn release_turn_cancel_closure_participant(
        &self,
        _participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        if scope.session_id().is_some() {
            return Ok(());
        }
        Err(RuntimeError::new(
            RuntimeErrorCode::EffectJournalRetirementUnsupported,
            "this effect host does not implement cancellation-closure lifecycle participation",
        ))
    }
}

/// A session catalog's stable registration at the physical promise owner.
///
/// Catalog authorization first registers this participant at the owner and
/// then commits its local pin. Scope retirement reverses that order: it first
/// fences and drains the catalog, then releases the participant. This ordering
/// leaves only conservative retained participants across crashes, never an
/// unprotected authorization.
#[derive(Clone)]
pub struct TurnCancelClosureOwnerBinding {
    participant_id: Arc<str>,
    owner: Arc<dyn EffectHost>,
}

impl std::fmt::Debug for TurnCancelClosureOwnerBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TurnCancelClosureOwnerBinding")
            .field("participant_id", &self.participant_id)
            .finish_non_exhaustive()
    }
}

impl TurnCancelClosureOwnerBinding {
    pub fn new(participant_id: impl Into<Arc<str>>, owner: Arc<dyn EffectHost>) -> Self {
        Self {
            participant_id: participant_id.into(),
            owner,
        }
    }

    pub async fn register(
        &self,
        scope: &ExecutionScope,
        admitted_binding_id: &str,
    ) -> Result<(), RuntimeError> {
        let owner_binding_id =
            super::turn_control_binding_id_for_scope(&self.owner.turn_control_binding_id(), scope)?;
        if owner_binding_id != admitted_binding_id {
            return Err(RuntimeError::new(
                RuntimeErrorCode::InvalidTurnCancelRequest,
                format!(
                    "session catalog cancellation owner `{owner_binding_id}` does not match admitted authority `{admitted_binding_id}`"
                ),
            ));
        }
        self.owner
            .register_turn_cancel_closure_participant(&self.participant_id, scope)
            .await
    }

    pub async fn release(&self, scope: &ExecutionScope) -> Result<(), RuntimeError> {
        self.owner
            .release_turn_cancel_closure_participant(&self.participant_id, scope)
            .await
    }
}

/// Boundary for nondeterministic runtime work.
#[async_trait::async_trait]
pub trait RuntimeEffectController: AwaitEventResolver {
    /// Store-backed replay controllers leave this false: durable journal participation alone
    /// does not imply engine-owned backpressure.
    fn owns_commit_backpressure(&self) -> bool {
        false
    }

    /// Whether this engine pins a foreground turn's invocation to the build
    /// that started it (FIG-4739), so a turn on a draining build must end at
    /// its next quiet point for its run to go on in a new invocation on the
    /// newest build. A turn records a read of its build's drain mark at each
    /// quiet point only on an engine that answers `true`; on every other
    /// engine the next attempt of the turn already runs on whichever build
    /// is serving, and nothing is read.
    fn hands_over_turns(&self) -> bool {
        false
    }

    /// The substrate attempt this controller is executing under, if the
    /// substrate has one: the span context and invocation id its transport
    /// delivered for the attempt now running the handler.
    ///
    /// Freshly emitted observations link it; it is never a parent, never a
    /// cause and never stored. A substrate with no attempt notion keeps the
    /// default. Forwarding wrappers forward.
    fn attempt_observation(&self) -> Option<lash_trace::AttemptObservation> {
        None
    }

    /// Advises an engine to end the current in-process execution segment at a
    /// quiescent point. Engines may decline when live state is not capturable,
    /// but must make progress before returning another decline. In particular,
    /// an engine must not repeatedly return the same boundary and unchanged
    /// durable-wait state in one invocation; a host may bound and retry such a
    /// non-progressing invocation.
    fn wants_segment_boundary(&self, _progress: &SegmentProgress) -> Option<BoundaryReason> {
        None
    }

    /// Whether the process execution this controller executes has a committed
    /// cancellation, observed as a recorded operation (FIG-3673).
    ///
    /// A process's cancellation is a durable first-writer fact, and its shift
    /// observes it only through recorded operations: a race on each durable
    /// wait it records, this peek at each cancel checkpoint of its body and
    /// before its first command, and this peek once after a `SessionTurn`
    /// runner settles. An engine that records the fact answers from it and
    /// never reads `lent_stop`, so a replay reads the answer the first
    /// execution read.
    ///
    /// `lent_stop` is the stop the execution lends its step bodies. A
    /// controller that records no process cancellation fact (the SQL driver
    /// and the test controllers, which FIG-3668 deletes) answers from it: it is
    /// that controller's only cancellation input, and its journal is keyed, not
    /// positional. Forwarding wrappers forward.
    async fn observe_process_cancel(
        &self,
        lent_stop: &CancellationToken,
    ) -> Result<bool, RuntimeEffectControllerError> {
        Ok(lent_stop.is_cancelled())
    }

    /// Run one registry step of a process drive whose answer the engine
    /// records under `name` (FIG-3673): a process body's wait-state writes.
    ///
    /// An engine that replays its journal by position runs the step once and
    /// serves its recorded answer on every redrive, so a redrive that meets a
    /// registry the first execution has since moved on (a stored terminal)
    /// takes the path the first execution took. A retryable fault of the
    /// step is never recorded; the engine runs it again. The default runs the
    /// step: a keyed journal replays nothing positional around it. Forwarding
    /// wrappers forward.
    async fn record_process_drive_step(
        &self,
        name: String,
        step: ProcessDriveStep<'_>,
    ) -> Result<(), RuntimeEffectControllerError> {
        let _ = name;
        step.await.map_err(RuntimeEffectControllerError::from)
    }

    /// Journal one record of a logical Run's event log under `name` (K3,
    /// FIG-4877), and answer the entry the journal holds.
    ///
    /// An engine that replays its journal by position runs `step` once and
    /// serves the journaled entry on every redrive without running it again:
    /// a recorded attempt never re-executes its body, and a recorded decision
    /// never observes cancellation again. An `Err` from the step is never
    /// journaled; it ends the attempt retryably, and the engine runs the step
    /// again under the same name and call identity. The default refuses: a
    /// controller that journals no Run record cannot host a Run-owned tool
    /// call. Forwarding wrappers forward.
    /// The scope-bound Run-record observer, when this engine journals Run records.
    fn run_record_observer(&self) -> Option<&crate::trace::RunRecordObserver> {
        None
    }

    async fn record_run_record(
        &self,
        name: String,
        step: RunRecordStep<'_>,
    ) -> Result<lash_core_store::tool_run::RunJournalEntry, RuntimeEffectControllerError> {
        drop(step);
        Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::EngineControlUnsupported,
            format!("this effect controller journals no Run record; `{name}` cannot be recorded"),
        ))
    }

    /// Record a parallel Run's selection and decision with an owned SDK closure.
    async fn record_run_schedule(
        &self,
        name: String,
        step: RunRecordStep<'_>,
    ) -> Result<crate::tool_run::RunJournalEntry, RuntimeEffectControllerError> {
        drop(step);
        Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::EngineControlUnsupported,
            format!("this controller records no owned Run schedule: {name}"),
        ))
    }

    /// Arm a call's source before its attempt receives the completion key.
    async fn arm_run_source(
        &self,
        descriptor: crate::tool_run::SourceDescriptor,
    ) -> Result<(), RuntimeEffectControllerError> {
        let _ = descriptor;
        Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::EngineControlUnsupported,
            "this controller cannot arm a Run source",
        ))
    }

    /// Attach the process terminal using the exact source admitted by the Run.
    async fn attach_run_process_terminal(
        &self,
        descriptor: crate::tool_run::SourceDescriptor,
    ) -> Result<(), RuntimeEffectControllerError> {
        let _ = descriptor;
        Err(crate::tool_run::SourceRefusal::NotArmed.into())
    }

    /// Read physical-cut authority without emitting or awaiting SDK commands.
    /// Called only inside a recorded native frame decision.
    async fn peek_run_cut(&self) -> Result<Option<BoundaryReason>, RuntimeEffectControllerError> {
        Ok(None)
    }

    /// Read the immutable seal selected by short segment subscriptions.
    async fn await_run_sources(
        &self,
        subscriptions: Vec<crate::tool_run::SourceSubscription>,
        cancel: TurnCancelWait,
    ) -> Result<(usize, crate::tool_run::SourceSeal), RuntimeEffectControllerError> {
        let _ = (subscriptions, cancel);
        Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::EngineControlUnsupported,
            "this controller cannot await Run sources",
        ))
    }

    /// Cancel at the source authority and return its actual winning seal.
    async fn cancel_run_source(
        &self,
        descriptor: crate::tool_run::SourceDescriptor,
    ) -> Result<crate::tool_run::SourceSeal, RuntimeEffectControllerError> {
        let _ = descriptor;
        Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::EngineControlUnsupported,
            "this controller cannot cancel a Run source",
        ))
    }

    /// Register one independently completing attempt now, in command order.
    /// Replay must register the recorded prefix before awaiting an older X.
    fn start_run_attempt<'run>(
        &'run self,
        name: String,
        step: crate::tool_dispatch::RunAttemptStep<'run>,
    ) -> crate::tool_dispatch::RunAttemptHandle<'run> {
        drop(step);
        crate::tool_dispatch::RunAttemptHandle {
            body: Box::pin(std::future::ready(())),
            result: Box::pin(async move {
                Err(RuntimeEffectControllerError::new(
                    RuntimeErrorCode::EngineControlUnsupported,
                    format!("this controller records no Run attempt: {name}"),
                ))
            }),
        }
    }

    /// Eagerly register one declared-start launch and discharge as a VM run.
    fn start_run_prepare<'run>(
        &'run self,
        name: String,
        step: crate::tool_dispatch::RunStartPrepareStep<'run>,
    ) -> crate::tool_dispatch::RunStepHandle<'run, crate::tool_dispatch::RunStartPrepared> {
        drop(step);
        crate::tool_dispatch::RunStepHandle {
            body: Box::pin(std::future::ready(())),
            result: Box::pin(async move {
                Err(RuntimeEffectControllerError::new(
                    RuntimeErrorCode::EngineControlUnsupported,
                    format!("this controller records no start preparation: {name}"),
                ))
            }),
        }
    }

    /// Send `request` to the realization service under its key and attach to
    /// the invocation the send created (ADR 0130). The send and the attach
    /// are issued at this call's position; the returned selectable's value is
    /// awaited only by a fresh schedule's selector.
    async fn issue_run_realization<'run>(
        &'run self,
        request: crate::tool_dispatch::RealizationRequest,
    ) -> Result<crate::tool_dispatch::IssuedRealization<'run>, RuntimeEffectControllerError> {
        let _ = request;
        Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::EngineControlUnsupported,
            "this controller issues no Run realization",
        ))
    }

    /// Attach to previously issued protected work without sending or executing it again.
    async fn attach_run_realization<'run>(
        &'run self,
        invocation_id: String,
    ) -> Result<
        crate::tool_dispatch::RunSelectable<'run, crate::tool_dispatch::RealizationReceipt>,
        RuntimeEffectControllerError,
    > {
        let _ = invocation_id;
        Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::EngineControlUnsupported,
            "this controller attaches no realization",
        ))
    }

    /// Register a durable retry backoff now, preserving its deadline on replay.
    fn start_run_retry(&self, backoff_ms: u64) -> crate::tool_dispatch::RunRetryTimer<'_> {
        let _ = backoff_ms;
        Box::pin(async {
            Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::EngineControlUnsupported,
                "this controller records no Run retry timer",
            ))
        })
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError>;

    /// The recorded-frontier read (FIG-3586): every journal row this
    /// controller's scope holds inside `range`, compared bytewise.
    ///
    /// A replayed language runtime issues it once per run, over its own key
    /// namespace, before any command leaves: it is how the run knows which
    /// commands the journal already holds, so that nothing is dispatched live
    /// while a recorded entry at or beyond the current command still exists.
    ///
    /// The default refuses: every controller journals its effects, and an
    /// empty answer from a journal that does hold rows is exactly the hole
    /// the fence exists to close. Forwarding wrappers forward.
    async fn read_recorded_journal(
        &self,
        range: &super::super::recorded_keys::RecordedKeyRange,
    ) -> Result<RecordedJournal, RuntimeEffectControllerError> {
        let _ = range;
        Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::RecordedJournalReadUnsupported,
            "this effect controller does not answer the recorded-frontier read; a replayed \
             language runtime cannot know which of its commands the journal holds",
        ))
    }
}

/// One registry step of a process drive, for
/// [`record_process_drive_step`](RuntimeEffectController::record_process_drive_step).
pub type ProcessDriveStep<'step> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(), crate::PluginError>> + Send + 'step>,
>;

/// One record of a logical Run, for
/// [`record_run_record`](RuntimeEffectController::record_run_record): the
/// future that produces the record and the canonical material it owns, or a
/// fault that ends the attempt unrecorded.
pub type RunRecordStep<'step> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<lash_core_store::tool_run::RunJournalEntry, String>>
            + Send
            + 'step,
    >,
>;

/// A controller's answer to
/// [`read_recorded_journal`](RuntimeEffectController::read_recorded_journal).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordedJournal {
    /// The journal rows the scope holds in the range, readable by key.
    Keys(super::super::recorded_keys::RecordedKeys),
    /// The host replays its journal by position and checks each entry's name
    /// as it goes (Restate's journal-mismatch check): a range read has nothing
    /// to add, because that positional check is already the fence — a command
    /// issued out of recorded order meets a recorded entry of another name
    /// before anything is dispatched.
    Positional,
}

#[cfg(test)]
#[path = "control/tests.rs"]
mod tests;
