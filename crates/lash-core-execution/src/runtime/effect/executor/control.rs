use crate::SessionId;
use crate::TurnId;
pub use lash_core_store::await_event_identity::*;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{AdmittedScope, RuntimeError, RuntimeErrorCode};

use super::super::envelope::{RuntimeEffectEnvelope, RuntimeEffectOutcome};
use super::super::group::{
    EffectGroupHandle, GroupSettlement, LoserPolicy, RankedGroupSettlement, RuntimeEffectGroup,
};
use super::TurnControlBinding;
use super::await_event_support::await_event_scope_not_retirable;
use super::{RuntimeEffectControllerError, RuntimeEffectLocalExecutor, TurnCancelWait};

mod handle;
pub use handle::RuntimeEffectControllerHandle;
pub use handle::{BoundaryReason, SegmentProgress};

mod lane;
use lash_core_effect::retirement;
pub mod scope;
pub mod task;
pub use lane::*;
pub use lash_core_effect::AwaitEventResolver;
pub use retirement::*;
pub use scope::facade_ops;
pub use scope::*;
pub use task::*;

/// Backend-level factory for scoped effect controllers.
#[async_trait::async_trait]
pub trait EffectHost: AwaitEventResolver {
    /// Stable identity of the physical authority that owns this host's reserved
    /// turn-control promises. Implementors must preserve it across client or
    /// handler recreation for as long as issued keys remain recoverable.
    fn turn_control_binding_id(&self) -> String;

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

    /// The group-child-bound twin of [`scoped_static`](Self::scoped_static)
    /// (ADR 0099 §4, FIG-3470).
    ///
    /// A controller minted here mints every semantic admission it serves —
    /// the child's nested attempts, its intents' sinks — *under* `binding`'s
    /// recorded child, fenced by the substrate's own arbitration: the SQL
    /// claim refuses the insert once the minting replay row's cancel
    /// disposition has committed, and the Restate handler asks the serialized
    /// group index. A bound controller is the only controller a group child's
    /// nested work may run through: an unbound one would admit semantic writes
    /// whose minting child was already cancel-decided.
    ///
    /// The default refuses rather than lending an unfenced controller, the
    /// same posture as
    /// [`commit_group_child_final`](RuntimeEffectController::commit_group_child_final):
    /// a host without substrate-owned admission has no group-child controller
    /// to lend.
    fn scoped_for_group_child(
        &self,
        _admitted: AdmittedScope,
        _binding: crate::GroupChildBinding,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        Err(
            super::effect_groups_unsupported("durable group-child admission binding")
                .into_runtime_error(),
        )
    }

    /// The durable closing/finalization seam over this host's group journal
    /// (ADR 0099 §7, FIG-3410): the recorded `closing` fact a `close` writes
    /// and the four-step cursor a finalizer advances.
    ///
    /// `None` on a tier that keeps no group row — Restate answers the same
    /// lifecycle through its engine-side `EffectGroupIndex` `Closed`/`Retired`
    /// states, which are the twin of this seam, so there is nothing to hand
    /// out. The SQL hosts answer with the shared driver's closing object.
    fn effect_group_closing(
        &self,
    ) -> Option<Arc<dyn super::super::group_closing::StoreEffectGroupClosing>> {
        None
    }

    /// Installs — or returns the already-installed — tool-child wiring for this
    /// host, and registers it as the host's group-executor resolver
    /// (ADR 0099 §2, FIG-2266).
    ///
    /// **Get-or-init, not register.** One effect host can back several
    /// runtimes, and there is exactly one live-opener registry per host: two
    /// registries would mean a turn registering its opener in one while the
    /// resolver consulted the other, and its children would never run. A
    /// caller therefore hands in a candidate and uses whatever comes back.
    ///
    /// `None` means this host routes no tool children — either because it
    /// implements no durable effect groups at all (the default here), or
    /// because a different resolver is already registered, which the
    /// conformance suites do deliberately. Neither is a failure: a host that
    /// does not route tool children simply has none, and the group's own
    /// refusal is what an operator sees if one is ever opened.
    fn install_tool_child_host(
        &self,
        candidate: Arc<super::super::tool_child_driver::ToolChildHost>,
    ) -> Option<Arc<super::super::tool_child_driver::ToolChildHost>> {
        let _ = candidate;
        None
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

    async fn prepare_tool_intent(
        &self,
        sink: &dyn ToolIntentOutcomeSink,
        identity: &crate::ToolIntentIdentity,
        intent: crate::ToolIntent,
    ) -> Result<ToolIntentPreparation, RuntimeError> {
        let guard = sink.lock_submission_gate(&identity.replay_key).await;
        let record =
            crate::ToolIntentSubmissionRecord::new(identity.clone(), intent).map_err(|error| {
                RuntimeError::new(
                    RuntimeErrorCode::RecordEncodingFailed,
                    format!("failed to hash tool-intent submission: {error}"),
                )
            })?;
        let admission = sink.admit(record).await?;
        Ok(ToolIntentPreparation::RuntimeOwned {
            admission,
            _guard: guard,
        })
    }

    async fn record_tool_intent_outcome(
        &self,
        sink: &dyn ToolIntentOutcomeSink,
        identity: &crate::ToolIntentIdentity,
        _submitted: crate::ToolIntent,
        outcome: crate::ToolIntentExecutionOutcome,
    ) -> Result<(), RuntimeError> {
        sink.complete_submission(identity, outcome).await
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

    /// Exact scopes whose authoritative retirement has committed but whose
    /// execution-artifact owner has not yet been acknowledged as severed in
    /// every configured artifact store.
    async fn pending_artifact_owner_retirements(
        &self,
    ) -> Result<Vec<ExecutionScope>, RuntimeError> {
        Ok(Vec::new())
    }

    /// Acknowledge completion of execution-artifact cleanup for `scope`.
    async fn complete_artifact_owner_retirement(
        &self,
        _scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

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

    /// Drives independent pieces of work that each issue effects on this
    /// controller, every one to completion, one at a time in the given order.
    ///
    /// Running them one at a time is what a journal replayed by position
    /// needs, and it is correct on every journal. Pieces that ran together
    /// would commit in whatever order they reached the journal, and a replay
    /// issuing them in another order would meet a recorded entry of another
    /// name (FIG-3671). A controller whose effects cannot be misread by the
    /// order they commit in overrides this to run the pieces concurrently.
    /// That covers a controller that finds each recorded effect by its replay
    /// key and one that records nothing. Forwarding wrappers forward.
    async fn drive_independent_effect_work<'work>(&self, work: Vec<IndependentEffectWork<'work>>) {
        for piece in work {
            piece.await;
        }
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

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError>;

    /// Open — or replay — a group of independently journaled child effects.
    ///
    /// Returns once the group is durably recorded, **not** when a child settles.
    ///
    /// The one parameter is the group itself: **envelopes, and nothing else**.
    /// A caller does not supply the code that runs a child, because a caller
    /// cannot: three of the four paths that execute a grouped child — a retry, a
    /// drain of a group whose caller is gone, and an engine tier's own child
    /// invocation — happen where no caller is in scope, so a host that could
    /// only run what a caller handed it could not honor the contract at all. The
    /// host resolves every child from its journaled envelope through its
    /// registered [`GroupExecutors`](super::super::group_drain::GroupExecutors),
    /// which is the same seam the loser drain resolves through, so one host has
    /// one answer to "what code runs this child" on every path.
    ///
    /// The `'static` property is unchanged and still ratified — children must
    /// outlive the caller's future under
    /// [`LoserPolicy::RunToCompletion`], and the borrow-scoped
    /// `RuntimeEffectLocalExecutor<'_>` taken by
    /// [`execute_effect`](Self::execute_effect) carries the one lifetime this
    /// contract exists to break — it just lives at the resolver now rather than
    /// at the argument.
    ///
    /// **A child with no runner is a routing fact, not an outcome.** On a
    /// **first open** a host resolves *all* of the group's children **before it
    /// records the group**, and refuses the whole open with a typed
    /// [`RuntimeEffectGroupShape`](crate::RuntimeErrorCode::RuntimeEffectGroupShape)
    /// error if any child resolves to `None`. Recording first and discovering
    /// the gap later would leave a recorded group holding a child that can never
    /// settle, so every rank above it is unservable and the caller waits forever
    /// — the same failure the retired executor-vec arity check existed to
    /// prevent, now unrepresentable by absence because there is no vec to
    /// misalign. **The refusal must journal nothing, including the group row**:
    /// a host that recorded the group and then refused would answer the retry as
    /// a reopen, and a reopen passes the miss through, so the second attempt
    /// would succeed around a child that can never settle — a strand one attempt
    /// later, which is worse than the refusal it replaced. On a **reopen** the
    /// same miss is not an open refusal: the group is already journaled, and a
    /// deployment that has lost one child's runner is the drain's `NoExecutor`
    /// case (ADR 0065).
    ///
    /// A host with **no registered resolver at all** is a different fact from a
    /// child it cannot route, and answers differently: it refuses all three
    /// methods with
    /// [`EffectGroupUnsupported`](crate::RuntimeErrorCode::EffectGroupUnsupported),
    /// built through
    /// [`effect_groups_unsupported`](super::effect_groups_unsupported). Such a
    /// refusal journals nothing.
    ///
    /// That refusal is now the *only* way a host says "no groups here".
    /// There was a `supports_effect_groups()` flag beside these three methods and a
    /// conformance law binding the two together; FIG-2266 deleted it.
    /// added no safety — a host can lie in a flag exactly as easily as in a
    /// method — and it could not see engine-side deployment facts anyway, so a
    /// missing service registration surfaced as a true answer to the wrong
    /// question. What replaces it is wiring: these methods carry no default
    /// body, so an out-of-tree controller either implements groups or refuses
    /// them in its own source, and the refusal it must write is the same typed
    /// error the law used to check for.
    ///
    /// A reopen must be fenced on group shape: a host that finds a recorded group
    /// under this key whose child count or wake rule differs from the group
    /// passed here must refuse rather than reopen, because a shrunk child vec
    /// under one key silently renumbers every rank above the truncation and the
    /// per-child envelope-hash fence cannot see it.
    ///
    /// The default errors loudly rather than mis-executing a group, matching
    /// [`AwaitEventResolver::cancel_await_events_for_session`]: an out-of-tree
    /// controller that has not implemented groups fails closed with a named
    /// error.
    async fn open_effect_group(
        &self,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError>;

    /// Register this controller's envelope-to-executor resolver, once.
    ///
    /// One controller has one answer to "what code runs this journaled grouped
    /// child", so a second registration of a *different* resolver is refused
    /// and re-registering the resolver already held is a no-op.
    ///
    /// Defaulted to the same `EffectGroupUnsupported` refusal the three group
    /// methods give, and for the same reason: a controller with nowhere to put
    /// a resolver is a controller that does no groups at all. It is defaulted
    /// rather than required because — unlike the three methods above, whose
    /// defaults FIG-2266 deleted — this is wiring a host performs *on* a
    /// controller, and a controller that does no groups has a correct and
    /// unambiguous answer to it.
    fn register_group_executors(
        &self,
        executors: Arc<dyn super::super::group_drain::GroupExecutors>,
    ) -> Result<(), RuntimeEffectControllerError> {
        let _ = executors;
        Err(super::effect_groups_unsupported(
            "this runtime effect controller",
        ))
    }

    /// The bound group-child controller for hosts that are thin projections
    /// over this controller and own no scope-minting surface of their own:
    /// the substrate answers with the same controller
    /// [`EffectHost::scoped_for_group_child`] would lend — every admission it
    /// serves minted under `binding` (FIG-3470). Defaults to the unsupported
    /// refusal: a controller without substrate-owned admission has no bound
    /// controller to lend.
    fn group_child_scoped_controller(
        &self,
        _admitted: AdmittedScope,
        _binding: crate::GroupChildBinding,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        Err(
            super::effect_groups_unsupported("durable group-child admission binding")
                .into_runtime_error(),
        )
    }

    /// Await the next settlement in the group's durable settlement order.
    ///
    /// The obligation, stated engine-portably: **settlement `n` of a group is a
    /// durable fact, and every replay observes the same child at position `n`.**
    /// A host must not re-derive position `n` by racing live children once `n`
    /// has been decided. How the fact is stored is the host's business — a SQL
    /// row, a Restate journal entry, a Temporal history event.
    ///
    /// Settlements are served by *rank* — the child holding the
    /// `(handle.consumed() + 1)`-th smallest sequence — never by literal sequence
    /// equality, because sequences are monotonic without being gapless.
    ///
    /// The handle is the sole cursor of record and is taken by `&mut`: an
    /// implementation calls [`EffectGroupHandle::advance`] on exactly the
    /// settlements it returns and keeps no per-caller consumption state of its
    /// own, which is what makes consumption exactly-once across a crash. See
    /// [`EffectGroupHandle`] for the full normative rule.
    ///
    /// Cancellation returns
    /// [`RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled`](crate::RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled)
    /// and leaves the cursor and the durable rank untouched, so a later await
    /// resumes at the same rank. Exhaustion has no code because it is the
    /// caller's arithmetic: check
    /// [`EffectGroupHandle::is_exhausted`] rather than awaiting past the last
    /// child.
    async fn await_next_settlement(
        &self,
        handle: &mut EffectGroupHandle,
        cancel: CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError>;

    /// Read the group's settlement at `rank` without advancing any caller
    /// cursor (ADR 0099 §8): the recorded terminal and the child's durable
    /// identity, or `None` when fewer than `rank` children have settled. The
    /// incorporation prefix record reads the journal through this seam —
    /// consumption order belongs to the handle, but an incorporated prefix is
    /// an opener fact that must not move a cursor to read.
    ///
    /// The default refuses on the same grounds as
    /// [`commit_group_child_final`](Self::commit_group_child_final): a
    /// controller that cannot read back a group's recorded ranks cannot carry
    /// the §6 incorporation record either.
    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<Option<RankedGroupSettlement>, RuntimeEffectControllerError> {
        let _ = (group_key, rank);
        Err(super::effect_groups_unsupported(
            "durable group settlement read",
        ))
    }

    /// Release the caller's interest in the group.
    ///
    /// Under [`LoserPolicy::RunToCompletion`] the remaining children keep
    /// running under host ownership and journal their own settlements; the host
    /// owns their redrive. Under [`LoserPolicy::Cancel`] the host cancels
    /// them and journals each cancellation as that child's terminal. Either way
    /// the caller may not observe further settlements.
    ///
    /// `disposition` may only **narrow** the one the group declared at open:
    /// resolve it through
    /// [`LoserPolicy::resolve_close`] and refuse a widening request. The
    /// declared disposition is authoritative — it is journaled with the group
    /// row, so a group abandoned by a crash before its close is drained under it
    /// too, and no policy is invented at drain time.
    ///
    /// Close is **idempotent**, and its failure is retryable. Taking the handle
    /// by value blocks reuse only in-process: the handle is `Deserialize`, so a
    /// crash between a successful close and the continuation commit means a
    /// replayed frame closes the same group again by construction. A second close
    /// under the same disposition must therefore succeed rather than raise on a
    /// healthy replay path.
    async fn close_effect_group(
        &self,
        handle: EffectGroupHandle,
        disposition: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError>;

    /// Commit one group child's final record at the §4 linearization point —
    /// the durable half of the child's final-attempt boundary.
    ///
    /// This is the write ADR 0099 §5's commit order rides on: the CAS that
    /// moves the child `pending → committed`, the allocation of its durable
    /// `commit_seq`, and the persistence of `drain_input` — the sealed data a
    /// recovery needs to finish the drain and projection rather than re-run
    /// the attempt — as **one decision under the substrate's own
    /// serialization**. Store backends run it inside a transaction fenced on
    /// the claiming lease; the Restate substrate runs it inside the serialized
    /// group index handler. There is deliberately **no read-then-write on this side of
    /// the boundary**: the substrate's serialization is the fence, so a
    /// cancel decision can never slip between an advisory read and the commit
    /// it was supposed to guard.
    ///
    /// Returns [`EffectGroupChildCommitOutcome::Committed`] with the allocated
    /// position, [`AlreadyCommitted`] with the recorded winner's position and
    /// drain input on an idempotent retry, or [`CancelDecided`] when the
    /// cancel disposition owns the point — in which case the caller writes
    /// nothing of its own.
    ///
    /// [`AlreadyCommitted`]: super::super::group_journal::EffectGroupChildCommitOutcome::AlreadyCommitted
    /// [`CancelDecided`]: super::super::group_journal::EffectGroupChildCommitOutcome::CancelDecided
    ///
    /// The default refuses rather than inventing an arbitration: a controller
    /// that cannot serialize the decision cannot host grouped tool children,
    /// and silently succeeding would be a fence that does not exist.
    async fn commit_group_child_final(
        &self,
        commit: super::super::group_journal::GroupChildFinalCommit,
    ) -> Result<
        super::super::group_journal::EffectGroupChildCommitOutcome,
        RuntimeEffectControllerError,
    > {
        let _ = commit;
        Err(super::effect_groups_unsupported(
            "durable group-child commit boundary",
        ))
    }

    /// Wait at the durable §5 barrier: resolve once no committed sibling
    /// below `commit_seq` in `group_key` still owes its drain.
    ///
    /// Drains are admitted in final-commit order, so the caller emits its
    /// nested semantic commands only once this resolves. The barrier is
    /// lifted by a sibling's drain, never by time, and the wait is the
    /// host's: a store-backed host parks on its journal's change
    /// notification for the group, and an engine-backed host on the engine's
    /// own durable wake for each blocking sibling's seat. Nothing here sleeps
    /// on a clock. The default refuses on the same grounds as
    /// [`commit_group_child_final`](Self::commit_group_child_final): a
    /// controller that cannot answer the durable barrier cannot order drains
    /// either.
    async fn await_group_child_drain_admission(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<(), RuntimeEffectControllerError> {
        let _ = (group_key, commit_seq);
        Err(super::effect_groups_unsupported("durable drain barrier"))
    }

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
        range: &super::super::effect_replay_driver::RecordedKeyRange,
    ) -> Result<RecordedJournal, RuntimeEffectControllerError> {
        let _ = range;
        Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::RecordedJournalReadUnsupported,
            "this effect controller does not answer the recorded-frontier read; a replayed \
             language runtime cannot know which of its commands the journal holds",
        ))
    }
}

/// One piece of work handed to
/// [`drive_independent_effect_work`](RuntimeEffectController::drive_independent_effect_work):
/// a future that issues effects on the controller and reports its result
/// through whatever it captured.
pub type IndependentEffectWork<'work> =
    std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'work>>;

/// A controller's answer to
/// [`read_recorded_journal`](RuntimeEffectController::read_recorded_journal).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordedJournal {
    /// The journal rows the scope holds in the range, readable by key.
    Keys(super::super::effect_replay_driver::RecordedKeys),
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
