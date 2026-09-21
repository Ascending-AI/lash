use crate::SessionId;
use crate::TurnId;
pub use lash_core_store::await_event_identity::*;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{RuntimeError, RuntimeErrorCode};

use super::super::envelope::{RuntimeEffectEnvelope, RuntimeEffectOutcome};
use super::super::group::{EffectGroupHandle, GroupSettlement, LoserPolicy, RuntimeEffectGroup};
use super::await_event_support::await_event_scope_not_retirable;
use super::{RuntimeEffectControllerError, RuntimeEffectLocalExecutor, TurnCancelWait};
use super::{TurnControlAuthorityOwner, TurnControlBinding, TurnControlParticipation};

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

/// Deployment-level factory for scoped effect controllers.
#[async_trait::async_trait]
pub trait EffectHost: AwaitEventResolver {
    /// Stable identity of the physical authority that owns this host's reserved
    /// turn-control promises. Implementors must preserve it across client or
    /// handler recreation for as long as issued keys remain recoverable.
    fn turn_control_binding_id(&self) -> String;
    /// Declares the owner of reserved turn-control promises for this host.
    fn turn_control_authority_owner(&self) -> TurnControlAuthorityOwner {
        TurnControlAuthorityOwner::EffectHost
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
    /// Durable hosts override this projection; native hosts use keyed promises through the host.
    fn turn_attach(&self) -> Option<Arc<dyn crate::TurnAttach>> {
        None
    }
    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError>;

    fn scoped_static(
        &self,
        _scope: ExecutionScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        Ok(None)
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
        match scoped.controller().turn_control_participation().await? {
            TurnControlParticipation::Local => {
                let resolver = self.await_event_resolver();
                Ok(TurnControlBinding::host_owned(
                    binding_id,
                    resolver,
                    self.scoped(scoped.execution_scope().clone())?,
                    self.turn_attach(),
                ))
            }
            TurnControlParticipation::DurableJournaled => {
                let resolver = scoped.controller();
                let Some(controller_authority_id) = resolver.await_event_authority_binding_id()
                else {
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
                    true,
                    self.turn_attach(),
                ))
            }
        }
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

    /// The SQLite database file holding this host's effect journal, when the
    /// journal lives in a file of its own that a session-store factory
    /// attaches for the retention sweep (ADR 0067). A host whose journal
    /// shares the store's database, keeps it in memory, or has no durable
    /// journal answers `None`.
    fn effect_scope_fence_database(&self) -> Option<std::path::PathBuf> {
        None
    }

    /// Bind the process registry that owns the process-scope fence
    /// (ADR 0049). Called by [`ProcessRegistrar::bind_effect_host`]
    /// (crate::ProcessRegistrar::bind_effect_host); idempotent.
    ///
    /// A host with a durable journal of its own in a file beside the
    /// registry's attaches [`ProcessRegistryBinding::fence_database`]
    /// (crate::ProcessRegistryBinding::fence_database) and reads and writes
    /// process-scope fences there, so registration's commit point and
    /// retirement's commit point are the same file. A host whose fence is a
    /// cache (the Restate durable-wait index) keeps
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
    /// Whether an engine owns pacing for commits made by this controller.
    /// Store-backed replay controllers leave this false: durable journal
    /// participation alone does not imply engine-owned backpressure.
    fn owns_commit_backpressure(&self) -> bool {
        false
    }

    async fn runtime_effect_failure_disposition(
        &self,
        _code: RuntimeErrorCode,
    ) -> Result<RuntimeEffectFailureDisposition, RuntimeError> {
        Ok(RuntimeEffectFailureDisposition::RecordTurnFailure)
    }

    async fn turn_control_participation(&self) -> Result<TurnControlParticipation, RuntimeError> {
        Ok(TurnControlParticipation::Local)
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

    /// Whether this controller can safely accept overlapping `execute_effect`
    /// calls from one runtime coordinator.
    ///
    /// Local and store-backed controllers can usually fan out independent
    /// effects. Some workflow substrates expose a single ordered journal
    /// context where native operations must be awaited immediately before the
    /// next context call is issued. Those controllers should return `false` so
    /// coordinators serialize child effects while still replaying each child by
    /// its own stable key.
    fn supports_concurrent_effects(&self) -> bool {
        true
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
    /// That refusal is now the *only* way a host says "no groups here". There
    /// was a `supports_effect_groups()` flag beside these three methods and a
    /// conformance law binding the two together; FIG-2266 deleted it. A boolean
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
}

#[cfg(test)]
#[path = "control/tests.rs"]
mod tests;
