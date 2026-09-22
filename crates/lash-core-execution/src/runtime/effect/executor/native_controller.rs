//! The in-memory reference host for the durable effect-group contract, and the
//! native controller it hangs off (FIG-1416, ADR 0065).
//!
//! `NativeRuntimeEffectController` stores no journal entries: lash re-executes
//! the local path on every attempt, and its only other state is an in-memory
//! await-event registry. So
//! this tier cannot and does not provide cross-process settlement durability —
//! the SQL stores own that half. What it *is* is the conformance definition of
//! the contract's **observable semantics**: wake behaviour, settlement order by
//! rank, loser completion under
//! [`LoserPolicy::RunToCompletion`](crate::LoserPolicy::RunToCompletion),
//! cancelled terminals under [`Cancel`](crate::LoserPolicy::Cancel), and
//! the close-side narrowing rule. A durable host that disagrees with this module
//! is wrong about the contract; a durable host that agrees with it and also
//! survives a crash is conformant.
//!
//! Three mechanisms carry that reference status and are worth reading as
//! normative rather than incidental:
//!
//! * **Children run on host-owned tasks, not inside the caller's future.** That
//!   is the structural break the contract exists for: dropping the caller must
//!   not drop the losers, because `RunToCompletion` says a losing promise keeps
//!   running and its side effects still happen.
//! * **The sequence is allocated at settlement, under the group's own lock, and
//!   never as a read-then-max.** This is the in-memory analogue of ADR 0065's
//!   single-row counter bump, and it is correct for the same reason: one
//!   allocation point, so two siblings settling at once cannot seat themselves
//!   at one rank. Allocating and recording under the same lock additionally
//!   makes recorded order *be* sequence order, so a rank read is an index.
//! * **A settlement is served from the record first and only otherwise awaited.**
//!   That is the whole determinism argument: once rank `n` has been decided it is
//!   re-read, never re-raced, so a caller that awaits rank `n` twice — either
//!   side of a park — observes the same child both times.
//! * **The group is the opener's supervisor (ADR 0099 §14).** It owns its
//!   children's tasks for exactly the group's life — open to reap, and reap
//!   happens only after the last settlement, so no task is ever aborted while
//!   a loser drains; under `Cancel` a child ends by observing the token and
//!   recording its terminal, never by abort. Each task owns the runner that
//!   owns the lent opener context through the child's whole drain, and the
//!   unsettled count below is what a quiescent retirement reads.

use crate::ProcessId;
use crate::SessionId;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_sansio::sync::{MutexExt, RwLockExt};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::super::await_events::AwaitEventRegistry;
use super::super::envelope::{
    ProcessCommand, RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectOutcome,
};
use super::super::group::EffectGroupDrainBudget;
use super::super::group::{
    EffectGroupHandle, EffectGroupRecordAccessor, GroupSettlement, GroupWakePolicy, LoserPolicy,
    RankedGroupSettlement, RuntimeEffectGroup, await_cancelled_error, child_cancelled_error,
    closed_group_error, exhausted_group_error, fence_reopen, group_shape_error,
};
use super::super::group_closing::{
    GroupFinalizationReport, GroupOnlyFinalization, OpenerFinalizationSteps,
    StoreEffectGroupClosing,
};
use super::super::group_drain::GroupExecutors;
use super::super::group_journal::{
    EffectGroupChildCommitOutcome, EffectGroupLifecycle, FinalizationStep, GroupChildFinalCommit,
};
use super::control::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, CompletionKeyPreparation,
    ExecutionScope, Resolution, ResolveOutcome, RuntimeEffectController,
};
use super::{
    RuntimeAwaitEventOptions, RuntimeEffectControllerError, RuntimeEffectLocalExecutor, task_panic,
};
use crate::{PluginError, ProcessRecord, RuntimeError, RuntimeErrorCode};

// =============================================================================
// Default in-process effect controller
// =============================================================================

/// Default in-process effect controller.
///
/// The native controller executes local runners in process and provides
/// in-memory await-event resolution. It does not make in-flight effects crash
/// durable; workflow adapters provide that by recording outcomes in history.
///
/// It is also the reference implementation of the durable effect-group
/// contract's observable semantics — see the module docs for what that does and
/// does not claim.
#[derive(Clone)]
pub struct NativeRuntimeEffectController {
    await_events: Arc<AwaitEventRegistry>,
    /// Group state lives beside the await-event registry rather than in a
    /// session store: a group host is an implementation *of* the effect-host
    /// contract, and the native substrate's contract is in-memory.
    groups: Arc<NativeEffectGroups>,
    process_lifetime_completion_keys_enabled: bool,
}

impl Default for NativeRuntimeEffectController {
    fn default() -> Self {
        Self {
            await_events: Arc::new(AwaitEventRegistry::new()),
            groups: Arc::new(NativeEffectGroups::default()),
            process_lifetime_completion_keys_enabled: false,
        }
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for NativeRuntimeEffectController {
    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        if !may_defer {
            return Ok(CompletionKeyPreparation::NotNeeded);
        }
        if !self.process_lifetime_completion_keys_enabled {
            return Ok(CompletionKeyPreparation::Unsupported);
        }
        self.await_event_key(scope, wait)
            .await
            .map(CompletionKeyPreparation::Issued)
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.await_events.key_for(scope, wait)
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.await_events.resolve(key, resolution)
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.await_events.peek_resolution(key)
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.await_events
            .await_resolution(key, cancel, deadline, &crate::SystemClock)
            .await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.await_events.revoke_session(session_id)
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.await_events.cancel_session(session_id)
    }

    async fn retire_await_events_for_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.await_events.retire_scope(scope)
    }

    async fn retire_await_events_for_scope_if_quiescent(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.await_events.retire_scope_if_quiescent(scope)
    }

    async fn reinstate_await_event_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.await_events.reinstate_scope(scope)
    }

    async fn await_event_scope_is_retired(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.await_events.scope_is_retired(scope)
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for NativeRuntimeEffectController {
    /// Until this is called the controller refuses all three group methods with
    /// [`EffectGroupUnsupported`](crate::RuntimeErrorCode::EffectGroupUnsupported),
    /// because the `'static` executors a child needs in order to outlive its
    /// caller have nowhere to come from. A second registration of a *different*
    /// resolver is refused: one host has one answer to what runs a child, and
    /// two would make the answer depend on when a path asked. Re-registering
    /// the resolver already held is a no-op.
    fn register_group_executors(
        &self,
        executors: Arc<dyn GroupExecutors>,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.groups.register_executors(executors)
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        match envelope.command {
            RuntimeEffectCommand::PeekAwaitEvent { key } => {
                let resolution = self
                    .await_events
                    .peek_resolution(&key)
                    .map_err(RuntimeEffectControllerError::from)?;
                Ok(RuntimeEffectOutcome::PeekAwaitEvent { resolution })
            }
            RuntimeEffectCommand::AwaitEvent { key } => {
                let RuntimeAwaitEventOptions {
                    cancellation,
                    deadline,
                    clock,
                    ..
                } = local_executor.into_await_event_options()?;
                let resolution = self
                    .await_events
                    .await_resolution(&key, cancellation, deadline, clock.as_ref())
                    .await
                    .map_err(RuntimeEffectControllerError::from)?;
                Ok(RuntimeEffectOutcome::AwaitEvent { resolution })
            }
            RuntimeEffectCommand::Process { command } => {
                if matches!(command.as_ref(), ProcessCommand::RegisterDefinition { .. }) {
                    let result = local_executor
                        .into_process_definitions()?
                        .execute(envelope.invocation.replay_key(), *command)
                        .await?;
                    return Ok(RuntimeEffectOutcome::Process { result });
                }
                let execution = local_executor.into_process()?;
                if matches!(command.as_ref(), ProcessCommand::Await { .. }) {
                    let result = execution.execute(*command).await?;
                    return Ok(RuntimeEffectOutcome::Process { result });
                }
                let result = task_panic::map_process_task_join(
                    crate::task::spawn(
                        lash_core_ids::execution_permit::inherit_process_execution_permit(
                            async move { execution.execute(*command).await },
                        ),
                    )
                    .await,
                )?;
                Ok(RuntimeEffectOutcome::Process { result })
            }
            RuntimeEffectCommand::Trigger { command } => {
                local_executor
                    .execute_trigger(envelope.invocation, *command)
                    .await
            }
            _ => local_executor.execute(envelope).await,
        }
    }

    /// Refused with
    /// [`EffectGroupUnsupported`](crate::RuntimeErrorCode::EffectGroupUnsupported)
    /// while no resolver is registered, exactly as
    /// [`await_next_settlement`](Self::await_next_settlement) and
    /// [`close_effect_group`](Self::close_effect_group) are: the flag and the
    /// three methods are one answer, and an unwired controller is a controller
    /// that does not implement groups.
    async fn open_effect_group(
        &self,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        group.validate_execution_scope(group.invocation().execution_scope())?;
        let executors = self.groups.registered_executors()?;
        NativeEffectGroups::open(&self.groups, &executors, group, self)
    }

    async fn await_next_settlement(
        &self,
        handle: &mut EffectGroupHandle,
        cancel: CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        self.groups.registered_executors()?;
        NativeEffectGroups::await_next_settlement(&self.groups, handle, cancel).await
    }

    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<Option<RankedGroupSettlement>, RuntimeEffectControllerError> {
        self.groups.registered_executors()?;
        NativeEffectGroups::read_settlement(&self.groups, group_key, rank)
    }

    /// Releases caller interest; finalization runs on a spawned task. The
    /// settled group is retained after it reaps — one entry per closed group
    /// until its scope retires through
    /// [`EffectHost::retire_effect_journal`](crate::EffectHost::retire_effect_journal)
    /// — so a reopen of the same key serves the recorded settlements whether
    /// or not the finalizer has reaped yet, instead of re-executing children
    /// this journal-less tier cannot replay (FIG-3548).
    async fn close_effect_group(
        &self,
        handle: EffectGroupHandle,
        disposition: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.groups.registered_executors()?;
        NativeEffectGroups::close(&self.groups, &handle, disposition)
    }

    /// The §4 boundary under the group's own lock — the same lock `record` and
    /// `close` race for the arbitration decision, so a commit here and a
    /// cancel-seat there cannot interleave a second writer between them.
    ///
    /// Membership resolves before the executor gate: a caller whose replay
    /// key belongs to no open group is answered `Ungrouped` — the honest
    /// answer — even on a controller that hosts no groups at all, because
    /// ordinary tool settlements ask this question of every attempt.
    async fn commit_group_child_final(
        &self,
        commit: GroupChildFinalCommit,
    ) -> Result<EffectGroupChildCommitOutcome, RuntimeEffectControllerError> {
        if self.groups.member_of(&commit.replay_key).is_none() {
            return Ok(EffectGroupChildCommitOutcome::Ungrouped);
        }
        self.groups.registered_executors()?;
        NativeEffectGroups::commit_group_child_final(&self.groups, commit)
    }

    async fn group_child_drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, RuntimeEffectControllerError> {
        self.groups.registered_executors()?;
        NativeEffectGroups::group_child_drain_blocked(&self.groups, group_key, commit_seq)
    }
}

impl NativeRuntimeEffectController {
    pub(in crate::runtime::effect) fn await_event_registry(&self) -> Arc<AwaitEventRegistry> {
        Arc::clone(&self.await_events)
    }

    /// The group supervisor table, so a `NativeEffectHost` over this
    /// controller can count unsettled children for a quiescent retirement.
    pub(in crate::runtime::effect) fn groups(&self) -> Arc<NativeEffectGroups> {
        Arc::clone(&self.groups)
    }

    pub(super) fn with_await_event_registry(await_events: Arc<AwaitEventRegistry>) -> Self {
        Self {
            await_events,
            groups: Arc::new(NativeEffectGroups::default()),
            process_lifetime_completion_keys_enabled: false,
        }
    }

    /// How long group finalization waits on a cancel-decided child's attempt
    /// body after its decision committed (ADR 0099 §7). A builder-time option
    /// like the SQL tiers' `*_EffectReplayOptions::drain_budget`: operational,
    /// never semantic.
    ///
    /// The groups table is empty at construction, so replacing it moves the
    /// budget and nothing else.
    pub fn drain_budget(mut self, budget: EffectGroupDrainBudget) -> Self {
        self.groups = Arc::new(NativeEffectGroups {
            drain_budget: budget,
            ..NativeEffectGroups::default()
        });
        self
    }

    /// Opt into externally routable keys that remain valid only while this
    /// controller's process and owned registry remain alive.
    pub fn allow_process_lifetime_completion_keys(mut self) -> Self {
        self.process_lifetime_completion_keys_enabled = true;
        self
    }
    /// Register the process and its initial observer edges into the durable registry.
    ///
    /// The native controller no longer runs the process here: the registry's
    /// non-terminal row *is* the durable work queue, and the host-owned
    /// [`ProcessWorkSubstrate`](crate::ProcessWorkSubstrate) is the sole executor.
    /// Registering the row is all this path does; the control seam drives the
    /// host driver after a successful start.
    /// Start, reporting whether the registry inserted the row or returned one
    /// it already held under the same registration fingerprint (FIG-3070).
    pub(crate) async fn start_process_reporting_realization(
        registry: Arc<dyn crate::ProcessRegistry>,
        registration: crate::ProcessRegistration,
        observers: Vec<SessionId>,
    ) -> Result<(ProcessRecord, crate::StoreRealization), PluginError> {
        let outcome = registry
            .register_process_reporting_disposition(registration, &observers)
            .await?;
        let realization = crate::StoreRealization::from_wrote(outcome.is_created());
        Ok((outcome.record, realization))
    }

    pub async fn request_process_cancel(
        registry: Arc<dyn crate::ProcessRegistry>,
        process_id: &ProcessId,
        origin: crate::CancelOrigin,
        requester: String,
        attribution: Option<crate::RuntimeReplayAttribution>,
    ) -> Result<ProcessRecord, PluginError> {
        let process_ref = registry.resolve_process_ref(process_id).await?;
        Self::request_process_cancel_ref(registry, &process_ref, origin, requester, attribution)
            .await
    }

    pub(crate) async fn request_process_cancel_ref(
        registry: Arc<dyn crate::ProcessRegistry>,
        process_ref: &crate::ProcessRef,
        origin: crate::CancelOrigin,
        requester: String,
        attribution: Option<crate::RuntimeReplayAttribution>,
    ) -> Result<ProcessRecord, PluginError> {
        registry
            .request_process_cancel(process_ref, origin, requester, attribution)
            .await
    }

    /// Cancel, reporting whether this call recorded the request or found the
    /// same cancellation already recorded (FIG-3070).
    pub(crate) async fn request_process_cancel_ref_reporting_realization(
        registry: Arc<dyn crate::ProcessRegistry>,
        process_ref: &crate::ProcessRef,
        origin: crate::CancelOrigin,
        requester: String,
        attribution: Option<crate::RuntimeReplayAttribution>,
    ) -> Result<(ProcessRecord, crate::StoreRealization), PluginError> {
        registry
            .request_process_cancel_reporting_realization(
                process_ref,
                origin,
                requester,
                attribution,
            )
            .await
    }
}

#[cfg(any(test, feature = "testing"))]
impl NativeRuntimeEffectController {
    /// The settlement record of one group, for the conformance tests that read
    /// what a durable tier would read out of its journal.
    pub fn recorded_group_settlements(&self, group_key: &str) -> Vec<RecordedSettlement> {
        self.groups.recorded(group_key)
    }

    /// The children still unsettled in open groups under `scope` — the count a
    /// quiescent-gated retirement reads on this tier.
    pub fn unsettled_children_under(&self, scope: &ExecutionScope) -> usize {
        self.groups.unsettled_children_under(scope)
    }

    /// The live task count of one open group — `None` once it has reaped.
    pub fn open_group_task_count(&self, group_key: &str) -> Option<usize> {
        self.groups.open_group_task_count(group_key)
    }

    /// How many reaped groups are retained for a reopen — each held until
    /// its scope retires (FIG-3548).
    pub fn retained_group_count(&self) -> usize {
        self.groups.retained_group_count()
    }
}

impl std::fmt::Debug for NativeRuntimeEffectController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeRuntimeEffectController").finish()
    }
}

// =============================================================================
// In-memory effect groups — the contract's semantic reference
// =============================================================================

/// One settlement as a test reads it: the child's position, the sequence it was
/// allocated, and whether its outcome was a success.
#[cfg(any(test, feature = "testing"))]
type RecordedSettlement = (usize, u64, bool);

/// Every group this controller has open, keyed exactly as ADR 0065 keys them.
///
/// A read-mostly index of per-group states, each with its own lock, rather than
/// one lock over everything: the per-group lock is the in-memory analogue of the
/// group row whose single-row counter bump ADR 0065 makes normative, and siblings
/// of *different* groups have no reason to contend.
#[derive(Default)]
pub(crate) struct NativeEffectGroups {
    open: std::sync::RwLock<HashMap<String, Arc<NativeEffectGroup>>>,
    /// This tier's one answer to "what code runs a grouped child", registered by
    /// the host that owns the runners. Absent until then, which is the same
    /// thing as this controller not supporting effect groups.
    executors: std::sync::OnceLock<Arc<dyn GroupExecutors>>,
    /// How long finalization waits on a cancel-decided child's attempt body
    /// after the decision committed — the same §7 bound the SQL tiers take at
    /// construction, set here by the controller builder.
    drain_budget: crate::runtime::effect::group::EffectGroupDrainBudget,
    /// Every reaped group, kept until its scope retires, so a reopen after
    /// close resurrects the recorded settlements rather than re-dispatching
    /// children whose effects already ran (FIG-3548). Without it a
    /// close→reopen would be served or re-executed depending on whether the
    /// spawned finalizer had reaped yet. One entry per closed group, evicted
    /// by [`evict_retired`](Self::evict_retired) when the host retires the scope.
    retired: Mutex<HashMap<String, Arc<NativeEffectGroup>>>,
}

/// One open group: its shape, its settlement record, and the wake that tells
/// blocked callers a new rank exists.
struct NativeEffectGroup {
    group_key: String,
    /// The scope the group was admitted under, which is what a quiescent
    /// retirement counts this group's unsettled children against (ADR 0099
    /// §14: the supervisor's unclaimed tasks are the scope's liveness).
    scope: ExecutionScope,
    children: usize,
    /// Held for reopen fencing only. The host deliberately does **not** branch on
    /// the wake rule: `await_next_settlement` serves rank `consumed + 1`
    /// whatever the rule says, and which settlement ends the caller's loop is a
    /// caller-side decision (`race` stops at the first, `any` at the first
    /// success, `all` at the first rejection, `allSettled` at none of them).
    /// A host that skipped failed settlements for `FirstSuccess` would make the
    /// losers' outcomes unreachable through the only method that reports them.
    wake: GroupWakePolicy,
    /// The disposition declared at open, which a crash-drain would apply.
    declared: LoserPolicy,
    /// The opener registration this group was opened under — the opener's
    /// identity *and* its registration generation. A re-registration of the
    /// same opener value is a new incarnation: this tier keeps no journal,
    /// so a group's recorded state lives exactly as long as the registration
    /// that produced it, and a reopen under a superseding registration must
    /// dispatch fresh rather than serve the dead context's answers.
    /// `None` when the resolver reports no registration — an unversioned
    /// group is always servable.
    opener_registration: Option<(crate::EffectOpener, u64)>,
    /// Fired by a close that resolves to [`LoserPolicy::Cancel`]. Children
    /// select on their own child token, so cancelling the group cancels exactly
    /// the children that have not settled.
    cancel: CancellationToken,
    /// The child tasks this group supervises: spawned at dispatch, joined by
    /// the JoinSet's own drop when the group is reaped after its last
    /// settlement. Owning them here — rather than detaching each on
    /// `task::spawn` — is what makes the group its children's supervisor.
    tasks: std::sync::Mutex<tokio::task::JoinSet<()>>,
    /// Replay key → position, so the §4 boundary commit can address a child by
    /// the identity its own envelope carries rather than by a position a
    /// caller could mistake.
    positions: HashMap<String, usize>,
    state: Mutex<NativeEffectGroupState>,
    settled: Notify,
    /// Fired when a child task *returns* — not when it settles, which `settled`
    /// already covers. Finalization's step-1 wait blocks on this: a
    /// cancel-decided task whose `record` early-returns still ends here, which
    /// is the signal the drain budget is a bound on.
    task_finished: Notify,
}

/// Which side of a child's §4 point committed, matching the durable tiers'
/// `decision` column. On this tier the point is the group lock itself:
/// `record` and `close` take it in whichever order they arrive, and the first
/// writer's decision is the arbitration fact the loser reads.
///
/// `Committed` carries no `commit_seq` of its own: this tier fuses commit and
/// seat under one lock, so a committed child's final-commit order is its
/// settlement rank — the same relation the Restate object records explicitly
/// and the SQL stores record as a separate column.
enum NativeChildDecision {
    /// The child's own final committed.
    Committed,
    /// The group's cancel disposition committed first; a late `record` reads
    /// this and drops the child's outcome rather than seating it twice.
    Cancelled,
}

struct NativeEffectGroupState {
    /// The allocation point. Bumped once per child, under this lock, at the
    /// moment that child settles — never computed as a max over siblings, which
    /// is the read-then-max shape ADR 0065 rejects because two siblings settling
    /// at once both read `k` and both write `k + 1`.
    next_sequence: u64,
    /// The §4 arbitration record: position → which side committed. A position
    /// appears here exactly when it is seated, so membership alone settles
    /// "has this child's point been decided"; the variant says by whom.
    decisions: HashMap<usize, NativeChildDecision>,
    /// The final-commit allocation point for children that win the §4 point at
    /// their attempt boundary rather than at `record`: bumped under this lock
    /// as each boundary commit lands, so two children racing the boundary seat
    /// at distinct positions — the `commit_seq` column's in-memory analogue.
    next_commit_seq: u64,
    /// position → (commit_seq, drain input) for every boundary-committed child
    /// and for direct `record` commits, which allocate their position here so
    /// the commit order is one column no matter which path wrote it.
    commits: HashMap<usize, (u64, Option<String>)>,
    /// Positions whose obligations have fully seated. A committed position not
    /// yet here is exactly what `group_child_drain_blocked` waits behind.
    drained: HashSet<usize>,
    /// Settled children in rank order. Allocating and appending under one lock
    /// makes append order sequence order, so rank *n* is `order[n - 1]` and the
    /// rank read is an index rather than a sort.
    order: Vec<NativeSettlement>,
    /// The group's lifecycle, mirroring the durable tiers' `lifecycle` column
    /// (ADR 0099 §7): `live` while children may claim, `closing` once close is
    /// recorded — written here *before* any cancel decision is seated, the same
    /// ordering §7 makes normative — and `settled` once finalization finished.
    /// The closing disposition it carries is the effective one, narrowed
    /// cumulatively like the `effective` field it replaces.
    lifecycle: EffectGroupLifecycle,
    closed: bool,
    /// Positions this controller dispatched that have not returned — the set
    /// finalization step 1 waits on: a `RunToCompletion` member of it is a
    /// protected obligation, a cancel-decided one is waited on only up to its
    /// drain budget.
    running: HashSet<usize>,
    /// Position → the instant its cancel decision committed. The drain budget
    /// is measured from the decision, per §7, not from the close.
    decided_at: HashMap<usize, Instant>,
}

struct NativeSettlement {
    position: usize,
    sequence: u64,
    outcome: Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
}

impl NativeSettlement {
    fn settlement(&self) -> GroupSettlement {
        GroupSettlement {
            position: self.position,
            sequence: self.sequence,
            outcome: self.outcome.clone(),
        }
    }
}

impl EffectGroupRecordAccessor for NativeEffectGroup {
    fn group_key(&self) -> &str {
        &self.group_key
    }

    fn children(&self) -> usize {
        self.children
    }

    fn wake(&self) -> GroupWakePolicy {
        self.wake
    }

    fn loser_disposition(&self) -> LoserPolicy {
        self.declared
    }
}

impl NativeEffectGroup {
    fn new(
        group: &RuntimeEffectGroup,
        opener_registration: Option<(crate::EffectOpener, u64)>,
    ) -> Self {
        Self {
            group_key: group.group_key().to_string(),
            scope: group.invocation().execution_scope().clone(),
            children: group.children().len(),
            wake: group.wake(),
            declared: group.loser_disposition(),
            opener_registration,
            cancel: CancellationToken::new(),
            positions: group
                .children()
                .iter()
                .enumerate()
                .map(|(position, child)| (child.invocation.replay_key().to_string(), position))
                .collect(),
            state: Mutex::new(NativeEffectGroupState {
                next_sequence: 0,
                decisions: HashMap::new(),
                next_commit_seq: 0,
                commits: HashMap::new(),
                drained: HashSet::new(),
                order: Vec::new(),
                lifecycle: EffectGroupLifecycle::Live,
                closed: false,
                running: HashSet::new(),
                decided_at: HashMap::new(),
            }),
            settled: Notify::new(),
            task_finished: Notify::new(),
            tasks: std::sync::Mutex::new(tokio::task::JoinSet::new()),
        }
    }

    /// Whether the opener registration that produced this group has been
    /// superseded by a re-registration of the same opener value.
    ///
    /// A reopened group whose registration is stale must dispatch its
    /// children again under the live registration's context: this tier keeps
    /// no journal, so serving the superseded registration's recorded
    /// settlements would stamp a dead context's answers under the new
    /// incarnation's name. A resolver that cannot report a generation — or
    /// an opener that is not live — leaves the record standing, since nobody
    /// else could run the children anyway.
    fn registration_stale(&self, executors: &Arc<dyn GroupExecutors>) -> bool {
        let Some((opener, generation)) = &self.opener_registration else {
            return false;
        };
        executors
            .live_generation(opener)
            .is_some_and(|live| live != *generation)
    }

    /// Closed with every child recorded: nothing in flight, so discarding
    /// the entry strands no running work.
    fn fully_settled(&self) -> bool {
        let inner = self.state.lock_recover();
        inner.closed && inner.order.len() == self.children
    }
}

/// The opener the group's children bind. Scope validation makes every child
/// share the group's scope, so the first tool child's opener is the group's;
/// a group carrying no tool children has no registration to compare.
fn group_opener(group: &RuntimeEffectGroup) -> Option<crate::EffectOpener> {
    group
        .children()
        .iter()
        .find_map(|child| match &child.command {
            RuntimeEffectCommand::ToolInvocation { request } => Some(request.scope.opener.clone()),
            _ => None,
        })
}

impl NativeEffectGroups {
    /// [`OnceLock::set`] is the arbiter rather than a preceding `get`: a
    /// get-then-set pair leaves a window in which two threads both read `None`
    /// and both believe they registered, and the loser's resolver would be
    /// silently discarded under an `Ok`. `set` decides, and its `Err` hands back
    /// the rejected resolver — identical to the held one means the caller
    /// re-registered what is already there, and anything else is the typed
    /// different-resolver refusal.
    #[expect(
        clippy::expect_used,
        reason = "a rejected set means the cell is already initialized"
    )]
    fn register_executors(
        &self,
        executors: Arc<dyn GroupExecutors>,
    ) -> Result<(), RuntimeEffectControllerError> {
        let Err(rejected) = self.executors.set(executors) else {
            return Ok(());
        };
        let held = self
            .executors
            .get()
            .expect("a rejected set means the lock is initialized");
        if Arc::ptr_eq(held, &rejected) {
            Ok(())
        } else {
            Err(group_shape_error(
                "this native effect controller already has a different \
                 registered group executor resolver; one host has one answer \
                 to what runs a grouped child",
            ))
        }
    }

    /// The registered resolver, or the refusal that says this controller does
    /// not implement groups at all.
    ///
    /// [`EffectGroupUnsupported`](crate::RuntimeErrorCode::EffectGroupUnsupported)
    /// rather than a shape refusal, because an unwired controller is not a
    /// controller with a bad group: it is a controller that does no groups at
    /// all, and it answers so through all three methods alike.
    fn registered_executors(
        &self,
    ) -> Result<Arc<dyn GroupExecutors>, RuntimeEffectControllerError> {
        self.executors.get().cloned().ok_or_else(|| {
            RuntimeEffectControllerError::new(
                RuntimeErrorCode::EffectGroupUnsupported,
                "this native effect controller has no registered group executor \
                 resolver, so it does not implement durable effect groups; \
                 register one with register_group_executors at wiring time",
            )
        })
    }

    fn resolve_children(
        executors: &Arc<dyn GroupExecutors>,
        group: &RuntimeEffectGroup,
    ) -> Result<Vec<RuntimeEffectLocalExecutor<'static>>, RuntimeEffectControllerError> {
        group
            .children()
            .iter()
            .enumerate()
            .map(|(position, child)| {
                executors.executor_for(child).ok_or_else(|| {
                    group_shape_error(format!(
                        "child {position} of durable effect group {} names a command \
                         this host has no runner for (replay key {}), so the group is refused before \
                         it is recorded: a group whose child can never settle holds a \
                        rank no settlement can take",
                        group.group_key(),
                        child.invocation.replay_key(),
                    ))
                })
            })
            .collect()
    }

    /// Every child is resolved through the registered [`GroupExecutors`] on the
    /// **first-open path**, before the group is recorded here and before any
    /// child is dispatched, and a child this host has no runner for refuses the
    /// whole open: a recorded group holding a child that can never settle owns a
    /// rank no settlement can take, and every rank above it is unservable.
    ///
    /// A reopen of a group this controller already holds resolves nothing and
    /// dispatches nothing: the children are already running under host
    /// ownership, re-running them would double every side effect the first
    /// dispatch is still producing, and resolving N executors only to drop them
    /// is work whose sole output would be a refusal for a group that is already
    /// open and settling. The returned handle is always at `consumed = 0`
    /// because only the caller knows how far it consumed; a caller resuming from
    /// a durable continuation restores its own cursor with
    /// [`EffectGroupHandle::restored`].
    ///
    /// The exception is a group whose opener registration has been
    /// superseded — a re-registered turn, a new process incarnation. The
    /// record the entry holds belongs to a dead context, and this tier keeps
    /// no journal, so the reopen dispatches the children fresh under the live
    /// registration rather than serving it.
    fn open(
        groups: &Arc<Self>,
        executors: &Arc<dyn GroupExecutors>,
        group: RuntimeEffectGroup,
        controller: &NativeRuntimeEffectController,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        let handle = EffectGroupHandle::new(&group);
        // The registration the open is happening under — the live
        // generation now, captured before resolution so the fresh state
        // records exactly the registration its children resolve through.
        let registration = group_opener(&group).and_then(|opener| {
            executors
                .live_generation(&opener)
                .map(|live| (opener, live))
        });
        {
            let mut open = groups.open.write_recover();
            if let Some(existing) = open.get(group.group_key()) {
                fence_reopen(&group, existing.as_ref())?;
                if existing.fully_settled() && existing.registration_stale(executors) {
                    // A superseded registration's completed record: dropped so
                    // the reopen falls through to a fresh dispatch.
                    open.remove(group.group_key());
                } else {
                    // A reopen is a new caller interest: an entry closed by an
                    // earlier caller but not yet reaped opens again — a closed
                    // group's settlements keep landing under host ownership
                    // precisely so a caller may read them. Only `closed` clears;
                    // the narrowed disposition stays cumulative. The clear runs
                    // under the map's write lock so a `reap` re-judging the entry
                    // under the same lock cannot retire it out from under the new
                    // handle.
                    existing.state.lock_recover().closed = false;
                    return Ok(handle);
                }
            }
            if groups.resurrect(&mut open, executors, &group)? {
                return Ok(handle);
            }
        }
        let resolved = Self::resolve_children(executors, &group)?;
        let state = {
            let mut open = groups.open.write_recover();
            if let Some(existing) = open.get(group.group_key()) {
                fence_reopen(&group, existing.as_ref())?;
                if existing.fully_settled() && existing.registration_stale(executors) {
                    open.remove(group.group_key());
                } else {
                    existing.state.lock_recover().closed = false;
                    return Ok(handle);
                }
            }
            if groups.resurrect(&mut open, executors, &group)? {
                return Ok(handle);
            }
            let state = Arc::new(NativeEffectGroup::new(&group, registration.clone()));
            open.insert(group.group_key().to_string(), Arc::clone(&state));
            state
        };
        Self::dispatch(groups, &state, group, resolved, controller);
        Ok(handle)
    }

    /// The task set is the host's, not the caller's: this is the structural break
    /// the contract exists for. A batch that owns its leaf futures inside the
    /// caller's future drops the losers when the caller is dropped, which is
    /// exactly what `RunToCompletion` says must not happen.
    fn dispatch(
        groups: &Arc<Self>,
        state: &Arc<NativeEffectGroup>,
        group: RuntimeEffectGroup,
        executors: Vec<RuntimeEffectLocalExecutor<'static>>,
        controller: &NativeRuntimeEffectController,
    ) {
        let group_key = Arc::<str>::from(group.group_key());
        for (position, (child, executor)) in
            group.children().iter().cloned().zip(executors).enumerate()
        {
            let groups = Arc::clone(groups);
            let state = Arc::clone(state);
            let group_key = Arc::clone(&group_key);
            let cancel = state.cancel.child_token();
            let task_owner = Arc::clone(&state);
            // Registered before the task exists, so a fast-finishing child
            // cannot remove a position that was never inserted.
            state.state.lock_recover().running.insert(position);
            let controller = controller.clone();
            let child_task = tracing::Instrument::instrument(
                async move {
                    // Dispatch through `execute_effect`, not the executor
                    // directly: some resolved executors are wait *options* an
                    // `execute` refuses (an `AwaitEvent` child), and this arm
                    // is where the tier reads them against its registry — the
                    // same shape `execute_effect_cancellable` gives the store
                    // tiers.
                    let execution = controller.execute_effect(child, executor);
                    tokio::pin!(execution);
                    let outcome = tokio::select! {
                        biased;
                        () = cancel.cancelled() => {
                            // A committed child retains authority to finish its
                            // drain (§4); the close decides only undecided
                            // children, so the token is not authorization here.
                            let committed = matches!(
                                state.state.lock_recover().decisions.get(&position),
                                Some(NativeChildDecision::Committed)
                            );
                            if committed {
                                execution.await
                            } else {
                                Err(child_cancelled_error(&group_key, position))
                            }
                        }
                        outcome = &mut execution => outcome,
                    };
                    Self::record(&groups, &group_key, &state, position, outcome);
                    {
                        // Removed before the notify so a finalizer woken by it
                        // reads the task as finished rather than still running.
                        let mut inner = state.state.lock_recover();
                        inner.running.remove(&position);
                        inner.decided_at.remove(&position);
                    }
                    state.task_finished.notify_waiters();
                },
                tracing::Span::current(),
            );
            task_owner.tasks.lock_recover().spawn(child_task);
        }
    }

    /// A position that already holds a settlement keeps it. That is what makes a
    /// close-time cancellation terminal and a child's own late completion resolve
    /// to one terminal rather than two ranks for one child.
    fn record(
        groups: &Arc<Self>,
        group_key: &str,
        state: &Arc<NativeEffectGroup>,
        position: usize,
        outcome: Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
    ) {
        let complete = {
            let mut inner = state.state.lock_recover();
            // The decision, not the seat: a position the cancel disposition
            // already committed drops its outcome here, and a position whose
            // own final committed keeps the first record — either way the §4
            // point admits exactly one writer. A boundary-committed position
            // (decision already `Committed`, not yet `drained`) is the child's
            // own final arriving to seat: it seats now and marks the drain
            // complete, since this tier has no post-commit intent window to
            // wait out.
            match inner.decisions.get(&position) {
                Some(NativeChildDecision::Cancelled) => return,
                Some(NativeChildDecision::Committed) if inner.drained.contains(&position) => {
                    return;
                }
                Some(NativeChildDecision::Committed) => {}
                None => {
                    inner
                        .decisions
                        .insert(position, NativeChildDecision::Committed);
                    inner.next_commit_seq += 1;
                    let commit_seq = inner.next_commit_seq;
                    inner.commits.insert(position, (commit_seq, None));
                }
            }
            inner.drained.insert(position);
            inner.next_sequence += 1;
            let sequence = inner.next_sequence;
            inner.order.push(NativeSettlement {
                position,
                sequence,
                outcome,
            });
            // Retirement additionally waits on the recorded lifecycle: a
            // closed-and-complete group whose finalizer has not yet written
            // `settled` still owns that write.
            inner.closed
                && inner.order.len() == state.children
                && matches!(inner.lifecycle, EffectGroupLifecycle::Settled { .. })
        };
        state.settled.notify_waiters();
        if complete {
            groups.reap(group_key, state);
        }
    }

    /// Serves rank `handle.consumed() + 1`, from the record first and only
    /// otherwise by waiting for it.
    async fn await_next_settlement(
        groups: &Arc<Self>,
        handle: &mut EffectGroupHandle,
        cancel: CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        let state = groups.lookup(handle.group_key())?;
        if handle.is_exhausted() {
            return Err(exhausted_group_error(handle));
        }
        loop {
            let notified = state.settled.notified();
            tokio::pin!(notified);
            let recorded = {
                let inner = state.state.lock_recover();
                if inner.closed {
                    return Err(closed_group_error(handle.group_key()));
                }
                inner
                    .order
                    .get(handle.consumed())
                    .map(NativeSettlement::settlement)
            };
            if let Some(settlement) = recorded {
                // The cursor advances on exactly the settlement returned, so an
                // await that is cancelled or refused leaves rank `n` to be read
                // again — by this caller or by a replayed frame — rather than
                // skipped.
                handle.advance()?;
                return Ok(settlement);
            }
            tokio::select! {
                () = cancel.cancelled() => {
                    return Err(await_cancelled_error(
                        handle.group_key(),
                        handle.consumed() + 1,
                    ));
                }
                () = &mut notified => {}
            }
        }
    }

    /// Serves the settlement recorded at `rank` without touching any caller
    /// cursor (ADR 0099 §8): the read a §6 incorporation prefix makes, which
    /// needs the child's durable identity rather than its declared position.
    fn read_settlement(
        groups: &Arc<Self>,
        group_key: &str,
        rank: u64,
    ) -> Result<Option<RankedGroupSettlement>, RuntimeEffectControllerError> {
        let state = groups.lookup(group_key)?;
        let inner = state.state.lock_recover();
        let index = usize::try_from(rank)
            .ok()
            .and_then(|rank| rank.checked_sub(1));
        let Some(settled) = index.and_then(|index| inner.order.get(index)) else {
            return Ok(None);
        };
        let child_replay_key = state
            .positions
            .iter()
            .find(|(_, position)| **position == settled.position)
            .map(|(replay_key, _)| replay_key.clone())
            .ok_or_else(|| {
                group_shape_error(format!(
                    "durable effect group {group_key} recorded a settlement at position \
                     {} that no member replay key names",
                    settled.position
                ))
            })?;
        Ok(Some(RankedGroupSettlement {
            sequence: settled.sequence,
            child_replay_key,
            outcome: settled.outcome.clone(),
        }))
    }

    /// Releases the caller's interest, applying the disposition the close
    /// resolves to.
    fn close(
        groups: &Arc<Self>,
        handle: &EffectGroupHandle,
        requested: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        // An already-reaped group is a closed group whose children have all
        // settled: it has no losers left for a disposition to decide, so a
        // replayed frame closing it again succeeds rather than raising on a
        // healthy replay path.
        let Some(state) = groups.get(handle.group_key()) else {
            return Ok(());
        };
        let cancelled = {
            let mut inner = state.state.lock_recover();
            // The lifecycle is written *before* any cancel decision is seated —
            // the same ordering ADR 0099 §7 makes normative on the durable
            // tiers, where `closing` is a journal fact rather than a flag —
            // so a reader can never observe a decision whose closing was not
            // already recorded.
            let recorded = inner
                .lifecycle
                .closing_disposition()
                .unwrap_or(state.declared);
            // Resolved against the disposition in force rather than the declared
            // one, so narrowing is cumulative: a group closed as `Cancel` cannot
            // be reopened to `RunToCompletion` by a second close either.
            let effective = LoserPolicy::resolve_close(recorded, requested)?;
            let finalized = match inner.lifecycle {
                EffectGroupLifecycle::Settled { .. } => return Ok(()),
                EffectGroupLifecycle::Live => FinalizationStep::NONE,
                EffectGroupLifecycle::Closing { finalized, .. } => finalized,
            };
            inner.lifecycle = EffectGroupLifecycle::Closing {
                disposition: effective,
                finalized,
            };
            inner.closed = true;
            let cancelled = matches!(effective, LoserPolicy::Cancel);
            if cancelled {
                let decided = Instant::now();
                // Each cancellation is decided and journaled as that child's
                // terminal, here and now rather than whenever the task notices,
                // so a caller that has closed under `Cancel` can read every
                // child's terminal immediately — and so a child that completes
                // in the cancellation window reads its `Cancelled` decision in
                // `record` and cannot claim a second rank.
                for position in 0..state.children {
                    if inner.decisions.contains_key(&position) {
                        continue;
                    }
                    inner
                        .decisions
                        .insert(position, NativeChildDecision::Cancelled);
                    // The drain budget is measured from the decision, per §7:
                    // the rank this seat owns is already durable, and the
                    // budget bounds how long finalization waits on the body.
                    if inner.running.contains(&position) {
                        inner.decided_at.entry(position).or_insert(decided);
                    }
                    inner.next_sequence += 1;
                    let sequence = inner.next_sequence;
                    inner.order.push(NativeSettlement {
                        position,
                        sequence,
                        outcome: Err(child_cancelled_error(handle.group_key(), position)),
                    });
                }
            }
            cancelled
        };
        if cancelled {
            state.cancel.cancel();
        }
        state.settled.notify_waiters();
        // Releasing caller interest never waits on finalization, but the
        // finalizer is this host's to drive — spawned here so a `close` that
        // returns cannot leave the recorded `closing` unworked.
        let finalize_groups = Arc::clone(groups);
        let finalize_key = handle.group_key().to_string();
        crate::task::spawn(async move {
            match NativeEffectGroups::finalize(
                &finalize_groups,
                &finalize_key,
                &GroupOnlyFinalization,
            )
            .await
            {
                Ok(report) => {
                    tracing::debug!(group_key = %finalize_key, ?report, "group finalization report")
                }
                Err(error) => tracing::warn!(
                    group_key = %finalize_key,
                    %error,
                    "group finalization failed; `closing` remains recorded for a \
                     resume to retry"
                ),
            }
        });
        Ok(())
    }

    /// The in-memory twin of the SQL tiers' four-step finalization (ADR 0099
    /// §7): same cursor, same ordering, same guarded retire — the only
    /// difference is that the lifecycle lives under the group's own lock
    /// instead of a `lifecycle` column, so the CAS is a read-modify-write
    /// taken under it.
    ///
    /// Step 1 waits on this controller's running children through
    /// `task_finished`: a `RunToCompletion` child unbounded (a protected
    /// obligation), a cancel-decided one only up to the drain budget measured
    /// from its decision. Steps 2 and 3 are the opener's, through
    /// `steps`. Step 4 writes `settled` and reaps through the same guarded
    /// check every other path uses.
    async fn finalize(
        groups: &Arc<Self>,
        group_key: &str,
        steps: &dyn OpenerFinalizationSteps,
    ) -> Result<GroupFinalizationReport, RuntimeEffectControllerError> {
        use crate::runtime::effect::GroupFinalizationReport as Report;
        let Some(state) = groups.get(group_key) else {
            // A group this controller does not hold was never opened or is
            // already reaped — either way there is nothing left to finalize.
            return Ok(Report::Settled {
                group_key: group_key.to_string(),
            });
        };
        loop {
            let step = {
                let inner = state.state.lock_recover();
                match inner.lifecycle {
                    EffectGroupLifecycle::Live => {
                        return Err(group_shape_error(format!(
                            "durable effect group {group_key} is live; only a \
                             group whose close is recorded can be finalized"
                        )));
                    }
                    EffectGroupLifecycle::Settled { .. } => {
                        return Ok(Report::Settled {
                            group_key: group_key.to_string(),
                        });
                    }
                    EffectGroupLifecycle::Closing { finalized, .. } => finalized.completed(),
                }
            };
            match step {
                0 => {
                    Self::await_local_obligations(&state, groups.drain_budget.duration()).await;
                    let unsettled = {
                        let inner = state.state.lock_recover();
                        state.children - inner.order.len()
                    };
                    if unsettled != 0 {
                        return Ok(Report::Pending {
                            group_key: group_key.to_string(),
                            unsettled,
                        });
                    }
                    Self::record_finalization_step(&state, 0);
                }
                1 => {
                    steps.commit_outcome_and_accounting(group_key).await?;
                    Self::record_finalization_step(&state, 1);
                }
                2 => {
                    steps.record_parent_end(group_key).await?;
                    Self::record_finalization_step(&state, 2);
                }
                _ => {
                    {
                        let mut inner = state.state.lock_recover();
                        if let EffectGroupLifecycle::Closing { disposition, .. } = inner.lifecycle {
                            inner.lifecycle = EffectGroupLifecycle::settled(disposition);
                        }
                    }
                    groups.reap(group_key, &state);
                    return Ok(Report::Settled {
                        group_key: group_key.to_string(),
                    });
                }
            }
        }
    }

    /// Record that finalization step `completed` (0-indexed) finished: advance
    /// the cursor under the group lock, but only while it still reads
    /// `completed` — a racing finalizer's further-along cursor is kept, which
    /// is the in-memory analogue of the SQL tiers' guarded CAS returning the
    /// value now durable.
    fn record_finalization_step(state: &Arc<NativeEffectGroup>, completed: u8) {
        let mut inner = state.state.lock_recover();
        if let EffectGroupLifecycle::Closing {
            disposition,
            finalized,
        } = inner.lifecycle
            && finalized.completed() == completed
            && let Some(next) = finalized.next()
        {
            inner.lifecycle = EffectGroupLifecycle::Closing {
                disposition,
                finalized: next,
            };
        }
    }

    /// The step-1 wait, on `task_finished` rather than a fixed sleep: a
    /// cancel-decided task is awaited only up to `budget` measured from its
    /// decision, and a `RunToCompletion` task — this controller's protected
    /// obligation — without bound. Recomputed on every wake, so a body that
    /// returned early stops being waited for the moment its notify lands.
    async fn await_local_obligations(state: &Arc<NativeEffectGroup>, budget: Duration) {
        loop {
            // A fresh `notified()` each pass: a `Notified` that already resolved
            // stays ready forever, so re-arming the same future would spin
            // instead of sleeping.
            let notified = state.task_finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let deadline = {
                let inner = state.state.lock_recover();
                let now = Instant::now();
                let mut latest: Option<Instant> = None;
                let mut waiting = false;
                for position in &inner.running {
                    match inner.decided_at.get(position) {
                        None => waiting = true,
                        Some(decided_at) => {
                            let expiry = *decided_at + budget;
                            if now < expiry {
                                waiting = true;
                                latest = Some(latest.map_or(expiry, |so_far| so_far.max(expiry)));
                            }
                            // Past its budget: logically cancelled — its seat
                            // is already recorded, and whatever the body still
                            // does cannot take it back.
                        }
                    }
                }
                if !waiting {
                    return;
                }
                latest
            };
            match deadline {
                Some(deadline) => {
                    tokio::select! {
                        () = &mut notified => {}
                        () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {}
                    }
                }
                None => notified.as_mut().await,
            }
        }
    }

    /// The §4 boundary commit, under the same lock `record` and `close` take:
    /// a child's final terminal commits its position in the group's
    /// final-commit order before its obligations drain, and a cancel that
    /// reached the position first refuses the late final instead.
    ///
    /// The child names itself by replay key and its membership resolves here —
    /// the durable row's `group_key` column analogue — never from a caller's
    /// assertion.
    fn commit_group_child_final(
        groups: &Arc<Self>,
        commit: GroupChildFinalCommit,
    ) -> Result<EffectGroupChildCommitOutcome, RuntimeEffectControllerError> {
        let Some((group_key, state)) = groups
            .member_of(&commit.replay_key)
            .and_then(|key| groups.get(&key).map(|state| (key, state)))
        else {
            return Ok(EffectGroupChildCommitOutcome::Ungrouped);
        };
        let position = state.positions[&commit.replay_key];
        let mut inner = state.state.lock_recover();
        match inner.decisions.get(&position) {
            Some(NativeChildDecision::Cancelled) => {
                Ok(EffectGroupChildCommitOutcome::CancelDecided {
                    group_key,
                    commit_seq: inner
                        .order
                        .iter()
                        .find(|seated| seated.position == position)
                        .map(|seated| seated.sequence)
                        .unwrap_or(0),
                })
            }
            Some(NativeChildDecision::Committed) => {
                let (commit_seq, drain_input) =
                    inner.commits.get(&position).cloned().unwrap_or((0, None));
                Ok(EffectGroupChildCommitOutcome::AlreadyCommitted {
                    group_key,
                    commit_seq,
                    drain_input,
                })
            }
            None => {
                inner.next_commit_seq += 1;
                let commit_seq = inner.next_commit_seq;
                inner
                    .decisions
                    .insert(position, NativeChildDecision::Committed);
                inner
                    .commits
                    .insert(position, (commit_seq, Some(commit.drain_input)));
                Ok(EffectGroupChildCommitOutcome::Committed {
                    group_key,
                    commit_seq,
                })
            }
        }
    }

    /// Whether a committed sibling below `commit_seq` still owes its seat —
    /// the in-memory analogue of the durable drain barrier.
    fn group_child_drain_blocked(
        groups: &Arc<Self>,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, RuntimeEffectControllerError> {
        let state = groups.lookup(group_key)?;
        let inner = state.state.lock_recover();
        Ok(inner
            .commits
            .iter()
            .any(|(position, (seq, _))| *seq < commit_seq && !inner.drained.contains(position)))
    }

    /// Which open group owns `replay_key`, if any — the membership lookup
    /// every child-addressed operation resolves through, so a caller's
    /// asserted group can never substitute for it.
    fn member_of(&self, replay_key: &str) -> Option<String> {
        self.open
            .read_recover()
            .iter()
            .find(|(_, state)| state.positions.contains_key(replay_key))
            .map(|(key, _)| key.clone())
    }

    fn get(&self, group_key: &str) -> Option<Arc<NativeEffectGroup>> {
        self.open.read_recover().get(group_key).cloned()
    }

    fn lookup(
        &self,
        group_key: &str,
    ) -> Result<Arc<NativeEffectGroup>, RuntimeEffectControllerError> {
        self.get(group_key)
            .ok_or_else(|| closed_group_error(group_key))
    }

    /// §4's admission fence on this tier: may a semantic effect minted under
    /// `binding`'s child still be admitted (ADR 0099 §4, FIG-3470)?
    ///
    /// The group lock this takes is the same one `record` and `close` write
    /// the decision under, so an admission can never observe a pre-decision
    /// state while its execution survives the decision's commit. A
    /// `Cancelled` decision refuses; anything else — `Committed`, or a
    /// position the boundary has not reached yet — admits, because a
    /// committed child retains authority to finish its drain and a live one
    /// has no decision to lose to.
    ///
    /// A group this controller does not hold is the closed-group error, the
    /// same answer `lookup` gives: a reaped group has no live state to
    /// arbitrate under, and a process crash is the only way this tier loses
    /// one. A binding that names a position the group never recorded is a
    /// shape refusal: the binding derives from a retained membership, so a
    /// replay key the group does not contain was never bound.
    pub(crate) fn admit_under(
        &self,
        binding: &crate::GroupChildBinding,
    ) -> Result<(), RuntimeEffectControllerError> {
        let group_key = binding.membership.group_key.as_str();
        let state = self.lookup(group_key)?;
        let Some(position) = state
            .positions
            .get(binding.child.replay_key.as_str())
            .copied()
        else {
            return Err(group_shape_error(format!(
                "group-child admission names replay key `{}`, which durable \
                 effect group {group_key} never recorded; a binding derives \
                 from the child's retained membership and cannot name a child \
                 the group does not contain",
                binding.child.replay_key,
            )));
        };
        let inner = state.state.lock_recover();
        if matches!(
            inner.decisions.get(&position),
            Some(NativeChildDecision::Cancelled)
        ) {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeEffectGroupChildCancelDecided,
                format!(
                    "child {position} of durable effect group {group_key} is \
                     cancel-decided; ADR 0099 §4 forbids a new semantic \
                     admission under it"
                ),
            ));
        }
        Ok(())
    }

    /// A group the reaper already retired still owes a reopening caller the
    /// settlements it recorded: reaping is a retention detail — not contract
    /// state — so until the group's scope retires the reopen resurrects the
    /// same state under the `open` map's write lock rather than
    /// re-dispatching children whose effects already ran. (The journaled
    /// tiers re-dispatch and let each child's claim replay; this tier
    /// journals nothing, so re-dispatch would re-execute.)
    ///
    /// The fence is judged before the group leaves `retired`, so a refused
    /// reopen costs the key nothing and `recorded` still answers for it. A
    /// record whose opener registration was superseded is dropped rather
    /// than resurrected, so the reopen dispatches under the live one.
    fn resurrect(
        &self,
        open: &mut HashMap<String, Arc<NativeEffectGroup>>,
        executors: &Arc<dyn GroupExecutors>,
        group: &RuntimeEffectGroup,
    ) -> Result<bool, RuntimeEffectControllerError> {
        let mut retired = self.retired.lock_recover();
        let Some(existing) = retired.get(group.group_key()) else {
            return Ok(false);
        };
        fence_reopen(group, existing.as_ref())?;
        let stale = existing.registration_stale(executors);
        let state = Arc::clone(existing);
        retired.remove(group.group_key());
        if stale {
            return Ok(false);
        }
        state.state.lock_recover().closed = false;
        open.insert(group.group_key().to_string(), state);
        Ok(true)
    }

    /// Completion alone is not enough, because `RunToCompletion` losers keep
    /// settling after the caller is gone and their settlements are the thing this
    /// state exists to record; a close alone is not enough for the same reason.
    ///
    /// So retention is bounded by close **and** completion, and a caller that
    /// drops its handle without ever closing leaves its group resident for the
    /// life of the controller — an unbounded key-space growth this tier does not
    /// defend against. That is survivable here and only here: the native
    /// controller is process-scoped and journals nothing, so the residue dies
    /// with the process rather than accumulating in a store. Closing is the
    /// caller's obligation under the contract; dropping a handle is not a
    /// supported way to end a group.
    fn reap(&self, group_key: &str, state: &Arc<NativeEffectGroup>) {
        let mut open = self.open.write_recover();
        if open.get(group_key).is_some_and(|current| {
            // Re-judge the retirement under the write lock: a reopen that
            // cleared `closed` — or a settlement that landed — since the
            // caller's check must not be retired out from under it.
            Arc::ptr_eq(current, state) && {
                let inner = current.state.lock_recover();
                inner.closed
                    && inner.order.len() == current.children
                    && matches!(inner.lifecycle, EffectGroupLifecycle::Settled { .. })
            }
        }) {
            open.remove(group_key);
            // The group itself, not a projection of it: a reopen resurrects
            // this state so the settlements it recorded are served rather
            // than re-run, until the scope retires.
            self.retired
                .lock_recover()
                .insert(group_key.to_string(), Arc::clone(state));
        }
    }

    /// Drop every settled record under a retiring scope (FIG-3548): the
    /// reaped groups in `retired`, and the closed, fully settled groups still
    /// in `open` whose finalizer has not reaped them yet — removed here under
    /// the same write lock `reap` takes, so a finalizer that finishes later
    /// finds nothing to retain. `retiring` answers which group scopes the
    /// retirement covers: one scope exactly, or every scope of a session.
    /// A reopen after a scope-exact retirement is refused by the scope fence;
    /// one after a session retirement opens a fresh group. Groups with caller interest or unsettled children stay: they are live
    /// work, not retained records.
    pub(in crate::runtime::effect) fn evict_retired(
        &self,
        retiring: impl Fn(&ExecutionScope) -> bool,
    ) {
        let mut open = self.open.write_recover();
        open.retain(|_, state| {
            if !retiring(&state.scope) {
                return true;
            }
            let inner = state.state.lock_recover();
            !(inner.closed && inner.order.len() == state.children)
        });
        self.retired
            .lock_recover()
            .retain(|_, state| !retiring(&state.scope));
    }

    /// The number of reaped groups this controller still retains, for the
    /// laws that pin retention to scope retirement.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn retained_group_count(&self) -> usize {
        self.retired.lock_recover().len()
    }

    /// The children still unsettled in open groups under `scope` — seats
    /// (`children`) minus recorded settlements (`order`). This is what a
    /// quiescent-gated retirement reads (ADR 0099 §14): a group closed under
    /// `RunToCompletion` keeps its draining losers counted until each records
    /// its terminal, so the fence cannot land mid-drain.
    pub(in crate::runtime::effect) fn unsettled_children_under(
        &self,
        scope: &ExecutionScope,
    ) -> usize {
        self.open
            .read_recover()
            .values()
            .filter(|state| state.scope == *scope)
            .map(|state| state.children - state.state.lock_recover().order.len())
            .sum()
    }

    /// The live task count of one open group, for the tests that pin the
    /// supervisor's ownership span. `None` once the group has reaped.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn open_group_task_count(&self, group_key: &str) -> Option<usize> {
        self.get(group_key)
            .map(|state| state.tasks.lock_recover().len())
    }

    /// The settlements recorded for a group, as `(position, sequence,
    /// settled_ok)` in rank order — the observation the contract's tests are
    /// written against.
    ///
    /// Falls back to the retired record so a group that reached its last
    /// settlement stays observable to a test: reaping is a retention decision,
    /// and a test that had to race it would be asserting on the reaper rather
    /// than on the contract.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn recorded(&self, group_key: &str) -> Vec<RecordedSettlement> {
        let Some(state) = self.get(group_key) else {
            return self
                .retired
                .lock_recover()
                .get(group_key)
                .map(|state| Self::snapshot(&state.state.lock_recover()))
                .unwrap_or_default();
        };
        let inner = state.state.lock_recover();
        Self::snapshot(&inner)
    }

    #[cfg(any(test, feature = "testing"))]
    fn snapshot(inner: &NativeEffectGroupState) -> Vec<RecordedSettlement> {
        inner
            .order
            .iter()
            .map(|settled| (settled.position, settled.sequence, settled.outcome.is_ok()))
            .collect()
    }

    /// The `closing` groups under `scope` — the set `resume_closing_groups`
    /// runs. `closing` because a `live` group owes no finalization and a
    /// `settled` one has none left.
    fn closing_groups_under(&self, scope: &ExecutionScope) -> Vec<Arc<NativeEffectGroup>> {
        self.open
            .read_recover()
            .values()
            .filter(|state| {
                state.scope == *scope
                    && matches!(
                        state.state.lock_recover().lifecycle,
                        EffectGroupLifecycle::Closing { .. }
                    )
            })
            .cloned()
            .collect()
    }
}

/// The [`StoreEffectGroupClosing`] over this tier's in-memory group table —
/// the same seam the SQL hosts hand out over their driver, so the W9–W12
/// laws run here too. There is no journal row, so "the durable read" is the
/// lifecycle under the group's own lock; everything else — cursor order,
/// budget wait, guarded retire — is identical.
pub(crate) struct NativeGroupClosing {
    groups: Arc<NativeEffectGroups>,
}

impl NativeGroupClosing {
    pub(crate) fn new(groups: Arc<NativeEffectGroups>) -> Self {
        Self { groups }
    }
}

#[async_trait::async_trait]
impl StoreEffectGroupClosing for NativeGroupClosing {
    async fn read_group_lifecycle(
        &self,
        group_key: &str,
    ) -> Result<Option<EffectGroupLifecycle>, RuntimeEffectControllerError> {
        Ok(self
            .groups
            .get(group_key)
            .map(|state| state.state.lock_recover().lifecycle))
    }

    async fn finalize_group(
        &self,
        group_key: &str,
        steps: &dyn OpenerFinalizationSteps,
    ) -> Result<GroupFinalizationReport, RuntimeEffectControllerError> {
        NativeEffectGroups::finalize(&self.groups, group_key, steps).await
    }

    async fn scope_is_quiescent(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeEffectControllerError> {
        // The in-memory twin: no journal rows exist here, so the scope's
        // still-live work reduces to the grouped children the supervisor
        // itself counts — a group short of a journaled child is an open seat,
        // and a closed one's draining losers stay counted until each records
        // its terminal (the same halves `scope_is_quiescent` covers on SQL).
        Ok(self.groups.unsettled_children_under(scope) == 0)
    }

    async fn resume_closing_groups(
        &self,
        scope: &ExecutionScope,
        steps: &dyn OpenerFinalizationSteps,
    ) -> Result<Vec<GroupFinalizationReport>, RuntimeEffectControllerError> {
        let mut reports = Vec::new();
        for state in self.groups.closing_groups_under(scope) {
            reports
                .push(NativeEffectGroups::finalize(&self.groups, &state.group_key, steps).await?);
        }
        Ok(reports)
    }
}
