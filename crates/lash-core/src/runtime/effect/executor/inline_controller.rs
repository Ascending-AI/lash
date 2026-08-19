//! The in-memory reference host for the durable effect-group contract, and the
//! inline controller it hangs off (FIG-1416, ADR 0065).
//!
//! `InlineRuntimeEffectController` stores no journal entries: its
//! `replay_ownership` is `Runtime`, lash re-executes the local path on every
//! attempt, and its only other state is an in-memory await-event registry. So
//! this tier cannot and does not provide cross-process settlement durability —
//! the SQL stores own that half. What it *is* is the conformance definition of
//! the contract's **observable semantics**: wake behaviour, settlement order by
//! rank, loser completion under
//! [`LoserDisposition::RunToCompletion`](crate::LoserDisposition::RunToCompletion),
//! cancelled terminals under [`Cancel`](crate::LoserDisposition::Cancel), and
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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lash_sansio::sync::{MutexExt, RwLockExt};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::super::await_events::AwaitEventRegistry;
use super::super::envelope::{
    ProcessCommand, RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectOutcome,
};
use super::super::group::{
    EffectGroupHandle, GroupSettlement, GroupWakePolicy, LoserDisposition, RuntimeEffectGroup,
};
use super::super::group_drain::GroupExecutors;
use super::control::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, ExecutionScope, Resolution,
    ResolveOutcome, RuntimeEffectController,
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
/// The inline controller executes local runners in process and provides
/// in-memory await-event resolution. It does not make in-flight effects crash
/// durable; workflow adapters provide that by recording outcomes in history.
///
/// It is also the reference implementation of the durable effect-group
/// contract's observable semantics — see the module docs for what that does and
/// does not claim.
#[derive(Clone)]
pub struct InlineRuntimeEffectController {
    await_events: Arc<AwaitEventRegistry>,
    /// Group state lives beside the await-event registry rather than in a
    /// session store: a group host is an implementation *of* the effect-host
    /// contract, and the inline tier's contract is in-memory.
    groups: Arc<InlineEffectGroups>,
    allow_process_lifetime_completion_keys: bool,
}

impl Default for InlineRuntimeEffectController {
    fn default() -> Self {
        Self {
            await_events: Arc::new(AwaitEventRegistry::new()),
            groups: Arc::new(InlineEffectGroups::default()),
            allow_process_lifetime_completion_keys: false,
        }
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for InlineRuntimeEffectController {
    fn allows_process_lifetime_completion_keys(&self) -> bool {
        self.allow_process_lifetime_completion_keys
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

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.await_events.revoke_session(session_id)
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.await_events.cancel_session(session_id)
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for InlineRuntimeEffectController {
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
                let execution = local_executor.into_process()?;
                if matches!(command.as_ref(), ProcessCommand::Await { .. }) {
                    let result = execution.execute(*command).await?;
                    return Ok(RuntimeEffectOutcome::Process { result });
                }
                let result = task_panic::map_process_task_join(
                    crate::task::spawn(
                        crate::runtime::process_worker::inherit_process_execution_permit(
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

    /// `true` exactly when a [`GroupExecutors`] resolver has been registered
    /// with [`register_group_executors`](Self::register_group_executors): the
    /// inline tier implements every observable semantic of the contract, and the
    /// resolver is where the `'static` executors the flag's other half requires
    /// come from, so a child can outlive its caller.
    ///
    /// It is a **per-deployment** fact, established at wiring time: before the
    /// registration this controller answers `false` and refuses all three group
    /// methods with
    /// [`EffectGroupUnsupported`](crate::RuntimeErrorCode::EffectGroupUnsupported),
    /// which is the coherence law the flag owes. A *per-child* resolver miss on
    /// a registered controller is a different fact and keeps its own typed
    /// routing refusal.
    ///
    /// The flag is not a durability claim and must not be read as one — that
    /// stays `replay_ownership`. What it asserts is that a group opened here runs
    /// its children durably *for the life of the runtime* and reports settlement
    /// order as a fact rather than re-racing it.
    fn supports_effect_groups(&self) -> bool {
        self.groups.executors.get().is_some()
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
        let executors = self.groups.registered_executors()?;
        InlineEffectGroups::open(&self.groups, &executors, group)
    }

    async fn await_next_settlement(
        &self,
        handle: &mut EffectGroupHandle,
        cancel: CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        self.groups.registered_executors()?;
        InlineEffectGroups::await_next_settlement(&self.groups, handle, cancel).await
    }

    async fn close_effect_group(
        &self,
        handle: EffectGroupHandle,
        disposition: LoserDisposition,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.groups.registered_executors()?;
        InlineEffectGroups::close(&self.groups, &handle, disposition)
    }
}

impl InlineRuntimeEffectController {
    /// Register the resolver that says what code runs a grouped child, once.
    ///
    /// Until this is called the controller answers
    /// [`supports_effect_groups`](RuntimeEffectController::supports_effect_groups)
    /// `false` and refuses all three group methods with
    /// [`EffectGroupUnsupported`](crate::RuntimeErrorCode::EffectGroupUnsupported),
    /// because the `'static` executors a child needs in order to outlive its
    /// caller have nowhere to come from. A second
    /// registration of a *different* resolver is refused: one host has one answer
    /// to what runs a child, and two would make the answer depend on when a path
    /// asked. Re-registering the resolver already held is a no-op.
    pub fn register_group_executors(
        &self,
        executors: Arc<dyn GroupExecutors>,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.groups.register_executors(executors)
    }

    /// Opt into externally routable keys that remain valid only while this
    /// controller's process and owned registry remain alive.
    pub fn allow_process_lifetime_completion_keys(mut self) -> Self {
        self.allow_process_lifetime_completion_keys = true;
        self
    }
    /// Register the process and its initial observer edges into the durable registry.
    ///
    /// The inline controller no longer runs the process here: the registry's
    /// non-terminal row *is* the durable work queue, and the host-owned
    /// [`ProcessWorkDriver`](crate::ProcessWorkDriver) is the sole executor.
    /// Registering the row is all this path does; the control seam drives the
    /// host driver after a successful start.
    pub(crate) async fn start_process(
        registry: Arc<dyn crate::ProcessRegistry>,
        registration: crate::ProcessRegistration,
        observers: Vec<String>,
    ) -> Result<ProcessRecord, PluginError> {
        registry
            .register_process_with_observers(registration, &observers)
            .await
    }

    pub(crate) async fn request_process_cancel(
        registry: Arc<dyn crate::ProcessRegistry>,
        process_id: &str,
        reason: Option<String>,
        replay: Option<crate::RuntimeReplay>,
    ) -> Result<ProcessRecord, PluginError> {
        // Cancellation is a durable signal: the cancel event is what the
        // runner-run process observes, so the inline controller appends it and
        // no longer tracks an in-process cancellation token.
        let mut request =
            crate::ProcessEventAppendRequest::cancel_requested(process_id, reason.clone());
        if let Some(replay) = replay {
            request = request.with_optional_replay(Some(replay));
        }
        registry.append_event(process_id, request).await?;
        registry
            .get_process(process_id)
            .await?
            .ok_or_else(|| PluginError::Session(format!("unknown process `{process_id}`")))
    }
}

#[cfg(test)]
impl InlineRuntimeEffectController {
    /// The settlement record of one group, for the conformance tests that read
    /// what a durable tier would read out of its journal.
    pub(crate) fn recorded_group_settlements(&self, group_key: &str) -> Vec<RecordedSettlement> {
        self.groups.recorded(group_key)
    }
}

impl std::fmt::Debug for InlineRuntimeEffectController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InlineRuntimeEffectController").finish()
    }
}

// =============================================================================
// In-memory effect groups — the contract's semantic reference
// =============================================================================

/// One settlement as a test reads it: the child's position, the sequence it was
/// allocated, and whether its outcome was a success.
#[cfg(test)]
type RecordedSettlement = (usize, u64, bool);

/// Every group this controller has open, keyed exactly as ADR 0065 keys them.
///
/// A read-mostly index of per-group states, each with its own lock, rather than
/// one lock over everything: the per-group lock is the in-memory analogue of the
/// group row whose single-row counter bump ADR 0065 makes normative, and siblings
/// of *different* groups have no reason to contend.
#[derive(Default)]
pub(crate) struct InlineEffectGroups {
    open: std::sync::RwLock<HashMap<String, Arc<InlineEffectGroup>>>,
    /// This tier's one answer to "what code runs a grouped child", registered by
    /// the host that owns the runners. Absent until then, which is the same
    /// thing as this controller not supporting effect groups.
    executors: std::sync::OnceLock<Arc<dyn GroupExecutors>>,
    /// The final record of every reaped group, so the reference tier's own
    /// tests can observe a completed group's settlement order. Test-only, in the
    /// style of `AwaitEventRegistry`'s cache counter: production keeps nothing
    /// once a group is reaped.
    #[cfg(test)]
    retired: Mutex<HashMap<String, Vec<RecordedSettlement>>>,
}

/// One open group: its shape, its settlement record, and the wake that tells
/// blocked callers a new rank exists.
struct InlineEffectGroup {
    children: usize,
    /// Held for reopen fencing only. The host deliberately does **not** branch on
    /// the wake rule: `await_next_settlement` serves rank `consumed + 1`
    /// whatever the rule says, and which settlement ends the caller's loop is a
    /// caller-side decision (`race` stops at the first, `any` at the first
    /// success, `all` at the first rejection, `allSettled` at none of them).
    /// A host that skipped failed settlements for `FirstSuccess` would make the
    /// losers' outcomes unreachable through the only method that reports them.
    wake: GroupWakePolicy,
    /// The disposition declared at open, which a crash-drain would apply. Close
    /// resolves against the *effective* disposition below, which starts here.
    declared: LoserDisposition,
    /// Fired by a close that resolves to [`LoserDisposition::Cancel`]. Children
    /// select on their own child token, so cancelling the group cancels exactly
    /// the children that have not settled.
    cancel: CancellationToken,
    state: Mutex<InlineEffectGroupState>,
    settled: Notify,
}

struct InlineEffectGroupState {
    /// The allocation point. Bumped once per child, under this lock, at the
    /// moment that child settles — never computed as a max over siblings, which
    /// is the read-then-max shape ADR 0065 rejects because two siblings settling
    /// at once both read `k` and both write `k + 1`.
    next_sequence: u64,
    /// Settled children in rank order. Allocating and appending under one lock
    /// makes append order sequence order, so rank *n* is `order[n - 1]` and the
    /// rank read is an index rather than a sort.
    order: Vec<InlineSettlement>,
    /// The disposition in force, which a close may narrow but never widen.
    effective: LoserDisposition,
    closed: bool,
}

struct InlineSettlement {
    position: usize,
    sequence: u64,
    outcome: Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
}

impl InlineSettlement {
    fn settlement(&self) -> GroupSettlement {
        GroupSettlement {
            position: self.position,
            sequence: self.sequence,
            outcome: self.outcome.clone(),
        }
    }
}

impl InlineEffectGroup {
    fn new(group: &RuntimeEffectGroup) -> Self {
        Self {
            children: group.children().len(),
            wake: group.wake(),
            declared: group.loser_disposition(),
            cancel: CancellationToken::new(),
            state: Mutex::new(InlineEffectGroupState {
                next_sequence: 0,
                order: Vec::new(),
                effective: group.loser_disposition(),
                closed: false,
            }),
            settled: Notify::new(),
        }
    }

    /// Refuses a reopen whose shape disagrees with the group already recorded
    /// under this key.
    ///
    /// A shrunk child vec under one key silently renumbers every rank above the
    /// truncation, and a changed wake rule or disposition is a changed journaled
    /// identity — neither of which the per-child envelope-hash fence can see
    /// from inside this process, because the inline tier records no envelopes.
    fn fence_reopen(&self, group: &RuntimeEffectGroup) -> Result<(), RuntimeEffectControllerError> {
        if self.children != group.children().len() {
            return Err(group_shape_error(format!(
                "durable effect group {} is open with {} children but was reopened \
                 with {}; a changed child count renumbers every rank above the \
                 change, so the settlements already consumed would no longer name \
                 the children that produced them",
                group.group_key(),
                self.children,
                group.children().len()
            )));
        }
        if self.wake != group.wake() {
            return Err(group_shape_error(format!(
                "durable effect group {} is open under wake rule {:?} but was \
                 reopened under {:?}; the wake rule is journaled identity and a \
                 reopen may not change it",
                group.group_key(),
                self.wake,
                group.wake()
            )));
        }
        if self.declared != group.loser_disposition() {
            return Err(group_shape_error(format!(
                "durable effect group {} declared loser disposition {:?} at open \
                 but was reopened declaring {:?}; the declared disposition is what \
                 a drain of this group applies, so a reopen may not restate it",
                group.group_key(),
                self.declared,
                group.loser_disposition()
            )));
        }
        Ok(())
    }
}

impl InlineEffectGroups {
    /// Registers this tier's envelope→executor resolver, once.
    ///
    /// [`OnceLock::set`] is the arbiter rather than a preceding `get`: a
    /// get-then-set pair leaves a window in which two threads both read `None`
    /// and both believe they registered, and the loser's resolver would be
    /// silently discarded under an `Ok`. `set` decides, and its `Err` hands back
    /// the rejected resolver — identical to the held one means the caller
    /// re-registered what is already there, and anything else is the typed
    /// different-resolver refusal.
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
                "this inline effect controller already has a different \
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
    /// controller with a bad group: it is a controller whose
    /// `supports_effect_groups` answers `false`, and the coherence law says the
    /// flag and the three methods must agree.
    fn registered_executors(
        &self,
    ) -> Result<Arc<dyn GroupExecutors>, RuntimeEffectControllerError> {
        self.executors.get().cloned().ok_or_else(|| {
            RuntimeEffectControllerError::new(
                RuntimeErrorCode::EffectGroupUnsupported,
                "this inline effect controller has no registered group executor \
                 resolver, so it does not implement durable effect groups; \
                 register one with register_group_executors at wiring time",
            )
        })
    }

    /// Resolves every child before anything is recorded, refusing the group if
    /// this host has no runner for one of them.
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
                         this host has no runner for{}, so the group is refused before \
                         it is recorded: a group whose child can never settle holds a \
                         rank no settlement can take",
                        group.group_key(),
                        child
                            .invocation
                            .replay_key()
                            .map(|key| format!(" (replay key {key})"))
                            .unwrap_or_default(),
                    ))
                })
            })
            .collect()
    }

    /// Opens — or reopens — a group, dispatching one host-owned task per child.
    ///
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
    fn open(
        groups: &Arc<Self>,
        executors: &Arc<dyn GroupExecutors>,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        let handle = EffectGroupHandle::new(&group);
        if let Some(existing) = groups.open.read_recover().get(group.group_key()).cloned() {
            existing.fence_reopen(&group)?;
            return Ok(handle);
        }
        let resolved = Self::resolve_children(executors, &group)?;
        let state = {
            let mut open = groups.open.write_recover();
            if let Some(existing) = open.get(group.group_key()) {
                existing.fence_reopen(&group)?;
                return Ok(handle);
            }
            let state = Arc::new(InlineEffectGroup::new(&group));
            open.insert(group.group_key().to_string(), Arc::clone(&state));
            state
        };
        Self::dispatch(groups, &state, group, resolved);
        Ok(handle)
    }

    /// Spawns each child on a host-owned task.
    ///
    /// The task set is the host's, not the caller's: this is the structural break
    /// the contract exists for. A batch that owns its leaf futures inside the
    /// caller's future drops the losers when the caller is dropped, which is
    /// exactly what `RunToCompletion` says must not happen.
    fn dispatch(
        groups: &Arc<Self>,
        state: &Arc<InlineEffectGroup>,
        group: RuntimeEffectGroup,
        executors: Vec<RuntimeEffectLocalExecutor<'static>>,
    ) {
        let group_key = Arc::<str>::from(group.group_key());
        for (position, (child, executor)) in
            group.children().iter().cloned().zip(executors).enumerate()
        {
            let groups = Arc::clone(groups);
            let state = Arc::clone(state);
            let group_key = Arc::clone(&group_key);
            let cancel = state.cancel.child_token();
            crate::task::spawn(async move {
                let outcome = tokio::select! {
                    biased;
                    () = cancel.cancelled() => Err(child_cancelled_error(&group_key, position)),
                    outcome = executor.execute(child) => outcome,
                };
                Self::record(&groups, &group_key, &state, position, outcome);
            });
        }
    }

    /// Records one child's settlement, allocating its sequence at that moment.
    ///
    /// A position that already holds a settlement keeps it. That is what makes a
    /// close-time cancellation terminal and a child's own late completion resolve
    /// to one terminal rather than two ranks for one child.
    fn record(
        groups: &Arc<Self>,
        group_key: &str,
        state: &Arc<InlineEffectGroup>,
        position: usize,
        outcome: Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
    ) {
        let complete = {
            let mut inner = state.state.lock_recover();
            if inner
                .order
                .iter()
                .any(|settled| settled.position == position)
            {
                return;
            }
            inner.next_sequence += 1;
            let sequence = inner.next_sequence;
            inner.order.push(InlineSettlement {
                position,
                sequence,
                outcome,
            });
            inner.closed && inner.order.len() == state.children
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
            return Err(group_shape_error(format!(
                "durable effect group {} has all {} settlements consumed; check \
                 is_exhausted() before awaiting rather than awaiting past the group",
                handle.group_key(),
                handle.children()
            )));
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
                    .map(InlineSettlement::settlement)
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
                    return Err(RuntimeEffectControllerError::new(
                        RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled,
                        format!(
                            "the await of settlement {} of durable effect group {} \
                             was cancelled; the group's rank is untouched and a \
                             later await resumes at the same settlement",
                            handle.consumed() + 1,
                            handle.group_key()
                        ),
                    ));
                }
                () = &mut notified => {}
            }
        }
    }

    /// Releases the caller's interest, applying the disposition the close
    /// resolves to.
    fn close(
        groups: &Arc<Self>,
        handle: &EffectGroupHandle,
        requested: LoserDisposition,
    ) -> Result<(), RuntimeEffectControllerError> {
        // An already-reaped group is a closed group whose children have all
        // settled: it has no losers left for a disposition to decide, so a
        // replayed frame closing it again succeeds rather than raising on a
        // healthy replay path.
        let Some(state) = groups.get(handle.group_key()) else {
            return Ok(());
        };
        let (cancelled, complete) = {
            let mut inner = state.state.lock_recover();
            // Resolved against the disposition in force rather than the declared
            // one, so narrowing is cumulative: a group closed as `Cancel` cannot
            // be reopened to `RunToCompletion` by a second close either.
            let effective = LoserDisposition::resolve_close(inner.effective, requested)?;
            inner.effective = effective;
            inner.closed = true;
            let cancelled = matches!(effective, LoserDisposition::Cancel);
            if cancelled {
                // Each cancellation is journaled as that child's terminal, here
                // and now rather than whenever the task notices, so a caller that
                // has closed under `Cancel` can read every child's terminal
                // immediately — and so a child that completes in the cancellation
                // window cannot claim a second rank.
                for position in 0..state.children {
                    if inner
                        .order
                        .iter()
                        .any(|settled| settled.position == position)
                    {
                        continue;
                    }
                    inner.next_sequence += 1;
                    let sequence = inner.next_sequence;
                    inner.order.push(InlineSettlement {
                        position,
                        sequence,
                        outcome: Err(child_cancelled_error(handle.group_key(), position)),
                    });
                }
            }
            (cancelled, inner.order.len() == state.children)
        };
        if cancelled {
            state.cancel.cancel();
        }
        state.settled.notify_waiters();
        if complete {
            groups.reap(handle.group_key(), &state);
        }
        Ok(())
    }

    fn get(&self, group_key: &str) -> Option<Arc<InlineEffectGroup>> {
        self.open.read_recover().get(group_key).cloned()
    }

    fn lookup(
        &self,
        group_key: &str,
    ) -> Result<Arc<InlineEffectGroup>, RuntimeEffectControllerError> {
        self.get(group_key)
            .ok_or_else(|| closed_group_error(group_key))
    }

    /// Drops a group once it is *both* closed and complete.
    ///
    /// Completion alone is not enough, because `RunToCompletion` losers keep
    /// settling after the caller is gone and their settlements are the thing this
    /// state exists to record; a close alone is not enough for the same reason.
    ///
    /// So retention is bounded by close **and** completion, and a caller that
    /// drops its handle without ever closing leaves its group resident for the
    /// life of the controller — an unbounded key-space growth this tier does not
    /// defend against. That is survivable here and only here: the inline
    /// controller is process-scoped and journals nothing, so the residue dies
    /// with the process rather than accumulating in a store. Closing is the
    /// caller's obligation under the contract; dropping a handle is not a
    /// supported way to end a group.
    fn reap(&self, group_key: &str, state: &Arc<InlineEffectGroup>) {
        let mut open = self.open.write_recover();
        if open
            .get(group_key)
            .is_some_and(|current| Arc::ptr_eq(current, state))
        {
            open.remove(group_key);
            #[cfg(test)]
            self.retired.lock_recover().insert(
                group_key.to_string(),
                Self::snapshot(&state.state.lock_recover()),
            );
        }
    }

    /// The settlements recorded for a group, as `(position, sequence,
    /// settled_ok)` in rank order — the observation the contract's tests are
    /// written against.
    ///
    /// Falls back to the retired record so a group that reached its last
    /// settlement stays observable to a test: reaping is a retention decision,
    /// and a test that had to race it would be asserting on the reaper rather
    /// than on the contract.
    #[cfg(test)]
    pub(crate) fn recorded(&self, group_key: &str) -> Vec<RecordedSettlement> {
        let Some(state) = self.get(group_key) else {
            return self
                .retired
                .lock_recover()
                .get(group_key)
                .cloned()
                .unwrap_or_default();
        };
        let inner = state.state.lock_recover();
        Self::snapshot(&inner)
    }

    #[cfg(test)]
    fn snapshot(inner: &InlineEffectGroupState) -> Vec<RecordedSettlement> {
        inner
            .order
            .iter()
            .map(|settled| (settled.position, settled.sequence, settled.outcome.is_ok()))
            .collect()
    }
}

fn group_shape_error(message: impl Into<String>) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(RuntimeErrorCode::RuntimeEffectGroupShape, message)
}

fn closed_group_error(group_key: &str) -> RuntimeEffectControllerError {
    group_shape_error(format!(
        "durable effect group {group_key} is closed to its caller; a closed group's \
         remaining children settle under host ownership and the caller may not \
         observe them"
    ))
}

fn child_cancelled_error(group_key: &str, position: usize) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        RuntimeErrorCode::RuntimeEffectGroupChildCancelled,
        format!(
            "child {position} of durable effect group {group_key} was cancelled by \
             the group's declared loser disposition; the cancellation is this \
             child's terminal"
        ),
    )
}
