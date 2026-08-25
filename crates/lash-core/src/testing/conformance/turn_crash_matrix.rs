//! Trace-derived crash coverage for one real scripted runtime turn.
//!
//! This suite instruments only integrator-owned seams: [`RuntimePersistence`],
//! [`crate::Provider`], and [`RuntimeEffectController`]. The runtime turn loop
//! has no test hooks or failpoints (ADR 0044). A reference turn containing
//! next-turn input, queued work, active-turn input at `AfterWork`, a model tool
//! call, and a final model response produces the committed golden trace. The
//! crash matrix is generated from that trace: every operation has a boundary
//! crash, every durable write has an inside-call lost-response crash, and the
//! scripted provider contributes its own mid-stream crash points. The scripted
//! provider deliberately holds the initial stream until one lease renewal is
//! observed, making the golden renewal position deterministic rather than a
//! claim about scheduler ordering.
//!
//! The matrix then replays the renewal-boundary point once more with the
//! recovered turn's lease-renewal task deterministically starved. A lease that
//! lapses by wall clock makes the checkpoint claim advisory: the undelivered
//! active-turn input is deferred to the next turn instead of being delivered,
//! and the suite drains it with one further turn and holds exactly-once on the
//! drained input. Pinning that path keeps it covered on every run rather than
//! only when a loaded runner happens to starve the renewal.
//!
//! Trace drift covers the operations explicitly decorated by this module.
//! Durable-store methods that [`SeamStore`] passes through undecorated are
//! outside that seam-coverage boundary until they are deliberately modeled.
//!
//! The outcome table is hand-written in `turn_crash_outcomes.json`. Its rulings
//! follow ADR 0029's reclaim-mediated LAW/NON-LAW split, ADR 0045's stateless
//! service rule, and the current-head CAS/floor semantics. In particular, a
//! crash after an external effect but before its outcome reaches the runtime
//! must re-execute that effect; this suite deliberately asserts at-least-once
//! behavior rather than fictional exactly-once suppression.
//!
//! Non-goals:
//!
//! - wake-delivery and trigger windows outside a running turn remain store-law
//!   responsibilities;
//! - sub-transaction torn writes belong to the substrate atomicity contract;
//! - in-process points between seam operations are durably equivalent to the
//!   next seam boundary: no durable fact can change between two seam calls, so
//!   killing anywhere in that interval recovers from the same durable prefix.
//! - level 1 uses task cancellation to check every generated semantic point;
//!   level 2 uses a separate process and `SIGKILL` at the selected durable-risk
//!   points.
//! - a level-2 known-defect ruling is not a skip: it requires a ticket and an
//!   exact defective durable end state. Any other state fails until the entry
//!   is consciously flipped to the exact correct state when the ticket lands.
//!
//! Integrator class: conformance-suite embedders (ADR 0051 class 4).

use lash_sansio::sync::MutexExt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::plugin::{PluginSpec, StaticPluginFactory};
use crate::provider::{Provider, ProviderComponents, ProviderHandle};
use crate::store::{PersistedSessionRead, RuntimeCommit, RuntimeCommitReceipt};
use crate::{
    CheckpointKind, LeaseOwnerIdentity, PendingTurnInput, PendingTurnInputDraft,
    QueuedWorkBatchDraft, QueuedWorkClaim, QueuedWorkClaimBoundary, RuntimeEffectController,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, RuntimePersistence, SessionExecutionLease,
    SessionExecutionLeaseAuthority, SessionExecutionLeaseClaimOutcome, SessionHeadMeta, StoreError,
    TurnInputClaim,
};

mod cold_process;

use cold_process::ColdProcessTurnAction;
pub use cold_process::{
    cold_process_durable_recovery_expectation, cold_process_real_turn_driver,
    cold_process_turn_expectations, cold_process_turn_scope,
};

const GOLDEN_TRACE: &str = include_str!("turn_crash_trace.json");
const OUTCOME_TABLE: &str = include_str!("turn_crash_outcomes.json");
const RECOVERY_TTL: Duration = Duration::from_millis(300);
const RECOVERY_RENEW: Duration = Duration::from_millis(100);
const NOMINAL_RECOVERY_TTL: Duration = Duration::from_secs(5);
const HIT_TIMEOUT: Duration = Duration::from_secs(60);
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
struct ReferenceIdentity {
    session_id: String,
    turn_id: String,
}

impl ReferenceIdentity {
    fn for_scenario(scenario: &str) -> Self {
        let session_id = format!("trace-derived-real-turn:{scenario}");
        let turn_id = format!("{session_id}:turn");
        Self {
            session_id,
            turn_id,
        }
    }
}

/// One typed operation shape observed at an integrator seam.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "seam", content = "operation")]
enum TurnSeamOperation {
    Store(StoreOperation),
    Provider(ProviderOperation),
    Effect(EffectOperation),
}

impl TurnSeamOperation {
    fn durable_write(&self) -> bool {
        matches!(
            self,
            Self::Store(
                StoreOperation::ClaimSessionExecutionLease
                    | StoreOperation::ClaimNextTurnInputs
                    | StoreOperation::ClaimReadyQueuedWork { .. }
                    | StoreOperation::ClaimSelectedQueuedWork { .. }
                    | StoreOperation::ClaimCheckpointWork { .. }
                    | StoreOperation::CommitFinalHead { .. }
                    | StoreOperation::RenewSessionExecutionLease
                    | StoreOperation::ReleaseSessionExecutionLease
            )
        )
    }
}

/// Semantic store calls crossed by the reference turn.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum StoreOperation {
    LoadSession,
    LoadSessionHeadMeta,
    ClaimSessionExecutionLease,
    RenewSessionExecutionLease,
    ReleaseSessionExecutionLease,
    ClaimLeadingSessionCommand,
    ClaimNextTurnInputs,
    DeferOrphanedActiveTurnInputs,
    ClaimReadyQueuedWork {
        boundary: String,
    },
    ClaimSelectedQueuedWork {
        boundary: String,
    },
    ClaimCheckpointWork {
        checkpoint: String,
    },
    CommitFinalHead {
        settles_queue: bool,
        settles_turn_input: bool,
        releases_lease: bool,
    },
}

/// Provider calls are identified from their semantic request content.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum ProviderOperation {
    InitialRequest,
    AfterToolRequest,
    InitialMidStream,
    AfterToolMidStream,
}

/// Effect calls are identified from the real envelope command.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum EffectOperation {
    ToolBatch { name: String },
    ToolAttempt { name: String },
}

/// Crash placement relative to the matched semantic operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CrashPlacement {
    Boundary,
    AfterExternalEffectBeforeOutcome,
    InsideCall,
    ProviderMidStream,
}

/// Stable generated crash-point identity.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
struct TurnCrashPoint {
    operation: TurnSeamOperation,
    placement: CrashPlacement,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Level2EffectExecutions {
    at_crash: usize,
    after_recovery: usize,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct DurableEndStateExpectation {
    #[serde(default)]
    terminal: Option<usize>,
    #[serde(default)]
    pending_inputs: Option<usize>,
    #[serde(default)]
    queued_work: Option<usize>,
}

impl DurableEndStateExpectation {
    fn exact(self) -> Option<DurableEndState> {
        Some(DurableEndState {
            terminal: self.terminal?,
            pending_inputs: self.pending_inputs?,
            queued_work: self.queued_work?,
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct KnownDefectExpectation {
    #[serde(default)]
    ticket: String,
    #[serde(default)]
    expected_defective: DurableEndStateExpectation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Level2Expectation {
    effect_executions: Level2EffectExecutions,
    #[serde(default)]
    exact: Option<DurableEndStateExpectation>,
    #[serde(default)]
    known_defect: Option<KnownDefectExpectation>,
}

/// Reviewable recovery ruling for one generated point.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct TurnCrashOutcome {
    point: TurnCrashPoint,
    outcome: String,
    effect_executions_l1: usize,
    #[serde(default)]
    level_2: Option<Level2Expectation>,
}

/// Reviewable durable end-state ruling for a composed level-2 trajectory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct DurableRecoveryRuling {
    scenario: String,
    outcome: String,
    exact: DurableEndStateExpectation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
enum ReviewedTurnCrashRuling {
    CrashPoint(TurnCrashOutcome),
    DurableRecovery(DurableRecoveryRuling),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DurableEndState {
    terminal: usize,
    pending_inputs: usize,
    queued_work: usize,
}

impl DurableEndState {
    const CORRECT: Self = Self {
        terminal: 1,
        pending_inputs: 0,
        queued_work: 0,
    };

    fn summary(self) -> String {
        format!(
            "terminal={} pending_inputs={} queued_work={}",
            self.terminal, self.pending_inputs, self.queued_work
        )
    }
}

#[derive(Debug, Default)]
struct SeamState {
    trace: Vec<TurnSeamOperation>,
    completed: Vec<TurnSeamOperation>,
    armed: Option<TurnCrashPoint>,
    hit: bool,
    process_crashed: bool,
}

#[derive(Clone, Debug, Default)]
struct SeamControl {
    state: Arc<Mutex<SeamState>>,
    hit: Arc<tokio::sync::Notify>,
    completed: Arc<tokio::sync::Notify>,
    /// When set, the scripted turn deterministically reproduces a starved
    /// lease-renewal task: the first renewal still completes (so the scripted
    /// provider's renewal park releases), every later renewal is held past the
    /// lease TTL, and the scripted provider holds its initial response until
    /// the wall-clock fence has certainly lapsed. See [`RenewalPressure`].
    starve_renewal: Arc<std::sync::atomic::AtomicBool>,
    /// When set, a background lease renewal is held until the scripted turn has
    /// reached its provider mid-stream point. See
    /// [`SeamControl::pin_renewal_after_provider`].
    pin_renewal_after_provider: Arc<std::sync::atomic::AtomicBool>,
    /// Notified on every [`SeamControl::record`], so a seam can wait for another
    /// seam to appear in the trace rather than poll for it.
    recorded: Arc<tokio::sync::Notify>,
}

/// Whether a matrix case runs its successor turn under a nominal lease-renewal
/// task or under a deterministically starved one.
///
/// A starved renewal is not a fault injected into the durable substrate: it is
/// the scheduling reality of a loaded CI runner, where the renewal task can
/// miss its deadline and the lease lapses by wall clock. The runtime answers
/// that with the advisory checkpoint skip (ADR 0029), which defers the
/// undelivered active-turn input to the next turn. `Starved` pins that path so
/// the matrix covers it on every run instead of only when the runner is slow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RenewalPressure {
    Nominal,
    Starved,
}

impl SeamControl {
    fn starve_renewals(&self) {
        self.starve_renewal
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn renewals_are_starved(&self) -> bool {
        self.starve_renewal
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Pin every background lease renewal behind the provider's mid-stream seam.
    ///
    /// **This exists solely for [`turn_crash_trace_drift_check`]'s exact
    /// golden-trace comparison, and no other test should reach for it.** That
    /// check is the one place a strict seam *ordering* is asserted; every matrix
    /// case asserts durable end states instead, and several of them arm crash
    /// points that legitimately stop the turn before the provider is ever
    /// called, where this park would have nothing to wait for.
    ///
    /// The race it removes: the renewal task fires on a fixed timer from the
    /// lease claim ([`RECOVERY_RENEW`], against a [`RECOVERY_TTL`] lease), while
    /// the turn has three store seams to clear before it reaches the provider.
    /// On a loaded runner the timer wins that race and the renewal is recorded
    /// ahead of `Provider(InitialRequest)` — a legal runtime ordering that a
    /// single golden trace cannot express. Ordering the renewal behind a
    /// positive signal removes the race without relaxing the comparison: the
    /// renewal must still happen, and the scripted provider still refuses to
    /// answer until it has.
    fn pin_renewal_after_provider(&self) {
        self.pin_renewal_after_provider
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Hold a renewal until the provider's mid-stream seam is in the trace.
    ///
    /// A no-op unless [`SeamControl::pin_renewal_after_provider`] armed it.
    async fn park_renewal_behind_provider(&self) {
        if !self
            .pin_renewal_after_provider
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let target = TurnSeamOperation::Provider(ProviderOperation::InitialMidStream);
        tokio::time::timeout(HIT_TIMEOUT, async {
            loop {
                let recorded = self.recorded.notified();
                if self.state.lock_recover().trace.contains(&target) {
                    break;
                }
                recorded.await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("pinned lease renewal never saw the provider reach {target:?}"));
    }

    fn record(&self, operation: TurnSeamOperation) {
        let mut state = self.state.lock_recover();
        let duplicate_renewal = operation
            == TurnSeamOperation::Store(StoreOperation::RenewSessionExecutionLease)
            && state.trace.contains(&operation);
        if !duplicate_renewal {
            state.trace.push(operation);
        }
        drop(state);
        self.recorded.notify_one();
    }

    fn arm(&self, point: TurnCrashPoint) {
        let mut state = self.state.lock_recover();
        state.trace.clear();
        state.completed.clear();
        state.armed = Some(point);
        state.hit = false;
        state.process_crashed = false;
    }

    fn clear(&self) {
        let mut state = self.state.lock_recover();
        state.trace.clear();
        state.completed.clear();
        state.armed = None;
        state.hit = false;
        state.process_crashed = false;
    }

    fn simulate_process_crash(&self) {
        self.state.lock_recover().process_crashed = true;
    }

    fn process_has_crashed(&self) -> bool {
        self.state.lock_recover().process_crashed
    }

    fn trace(&self) -> Vec<TurnSeamOperation> {
        self.state.lock_recover().trace.clone()
    }

    fn matches(&self, operation: &TurnSeamOperation, placement: CrashPlacement) -> bool {
        let mut state = self.state.lock_recover();
        if state.hit {
            return false;
        }
        let matches = state
            .armed
            .as_ref()
            .is_some_and(|point| point.operation == *operation && point.placement == placement);
        if matches {
            state.hit = true;
        }
        matches
    }

    async fn stop_here(&self) -> ! {
        // Store a permit when the spawned turn reaches the seam before its
        // parent starts waiting; `notify_waiters` would lose that signal.
        self.hit.notify_one();
        std::future::pending().await
    }

    async fn wait_for_hit(&self) {
        let armed = self.state.lock_recover().armed.clone();
        tokio::time::timeout(HIT_TIMEOUT, self.hit.notified())
            .await
            .unwrap_or_else(|_| panic!("armed semantic seam operation was not reached: {armed:?}"));
    }

    fn completed_count(&self, operation: &TurnSeamOperation) -> usize {
        self.state
            .lock_recover()
            .completed
            .iter()
            .filter(|completed| *completed == operation)
            .count()
    }

    fn mark_completed(&self, operation: TurnSeamOperation) {
        self.state.lock_recover().completed.push(operation);
        self.completed.notify_one();
    }

    async fn wait_until_completed(&self, operation: &TurnSeamOperation) {
        tokio::time::timeout(HIT_TIMEOUT, async {
            loop {
                let completed = self.completed.notified();
                if self.state.lock_recover().completed.contains(operation) {
                    break;
                }
                completed.await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("semantic seam operation did not complete: {operation:?}"));
    }

    async fn around<T, F>(&self, operation: TurnSeamOperation, future: F) -> T
    where
        F: Future<Output = T>,
    {
        self.record(operation.clone());
        if self.matches(&operation, CrashPlacement::Boundary) {
            self.stop_here().await;
        }
        let output = future.await;
        if self.matches(&operation, CrashPlacement::InsideCall) {
            self.stop_here().await;
        }
        self.mark_completed(operation);
        output
    }
}

struct SeamStore {
    inner: Arc<dyn RuntimePersistence>,
    control: SeamControl,
}

impl SeamStore {
    fn wrap(
        inner: Arc<dyn RuntimePersistence>,
        control: SeamControl,
    ) -> Arc<dyn RuntimePersistence> {
        Arc::new(Self { inner, control })
    }
}

#[async_trait::async_trait]
impl crate::store::RuntimePersistenceDecorator for SeamStore {
    fn inner(&self) -> &(dyn RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn load_session(&self) -> Result<Option<PersistedSessionRead>, StoreError> {
        self.control
            .around(
                TurnSeamOperation::Store(StoreOperation::LoadSession),
                self.inner.load_session(),
            )
            .await
    }

    async fn load_session_head_meta(&self) -> Result<Option<SessionHeadMeta>, StoreError> {
        self.control
            .around(
                TurnSeamOperation::Store(StoreOperation::LoadSessionHeadMeta),
                self.inner.load_session_head_meta(),
            )
            .await
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        let operation = TurnSeamOperation::Store(StoreOperation::CommitFinalHead {
            settles_queue: !commit.completed_queue_claims.is_empty(),
            settles_turn_input: !commit.completed_turn_input_claims.is_empty(),
            releases_lease: commit.release_session_execution_lease.is_some(),
        });
        self.control
            .around(operation, self.inner.commit_runtime_state(commit))
            .await
    }

    async fn claim_next_turn_inputs(
        &self,
        session_id: &str,
        fence: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<TurnInputClaim>, StoreError> {
        let operation = TurnSeamOperation::Store(StoreOperation::ClaimNextTurnInputs);
        self.control
            .around(
                operation,
                self.inner
                    .claim_next_turn_inputs(session_id, fence, owner, max_inputs),
            )
            .await
    }

    async fn defer_orphaned_active_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        scope: crate::OrphanedTurnInputScope<'_>,
    ) -> Result<crate::TurnCancelInputOutcome, StoreError> {
        let operation = TurnSeamOperation::Store(StoreOperation::DeferOrphanedActiveTurnInputs);
        self.control
            .around(
                operation,
                self.inner.defer_orphaned_active_turn_inputs(
                    session_id,
                    session_execution_lease,
                    scope,
                ),
            )
            .await
    }

    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        executor_id: &str,
        claim_nonce: &crate::LeaseClaimNonce,
        ttl: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        let operation = TurnSeamOperation::Store(StoreOperation::ClaimSessionExecutionLease);
        self.control
            .around(
                operation,
                self.inner.try_claim_session_execution_lease_with_token(
                    session_id,
                    owner,
                    executor_id,
                    claim_nonce,
                    ttl,
                ),
            )
            .await
    }

    async fn renew_session_execution_lease(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        ttl: u64,
    ) -> Result<SessionExecutionLease, StoreError> {
        let operation = TurnSeamOperation::Store(StoreOperation::RenewSessionExecutionLease);
        self.control.park_renewal_behind_provider().await;
        // A starved renewal task is modeled at its only observable seam: the
        // renewal call simply does not reach the store in time. The first
        // renewal is left intact because the scripted provider parks on it.
        //
        // The hold is scaled to the test's own hit timeout rather than to a
        // small multiple of the lease TTL: a checkpoint that lands late under a
        // loaded runner must still find the lease lapsed, and any hold short
        // enough for a re-armed renewal to beat it makes the case cover
        // nothing. The renewal task is aborted at release, so an over-long
        // hold costs no wall clock.
        if self.control.renewals_are_starved() && self.control.completed_count(&operation) >= 1 {
            tokio::time::sleep(HIT_TIMEOUT).await;
        }
        self.control
            .around(
                operation,
                self.inner.renew_session_execution_lease(fence, ttl),
            )
            .await
    }

    async fn release_session_execution_lease(
        &self,
        completion: &SessionExecutionLeaseAuthority,
    ) -> Result<(), StoreError> {
        if self.control.process_has_crashed() {
            return Err(StoreError::Backend(
                "simulated process crash suppresses owner-side lease release".to_string(),
            ));
        }
        let operation = TurnSeamOperation::Store(StoreOperation::ReleaseSessionExecutionLease);
        self.control
            .around(
                operation,
                self.inner.release_session_execution_lease(completion),
            )
            .await
    }

    // The diagnostic lease read inherits the delegating default deliberately:
    // observation is non-mutating and must never become a crash point.

    async fn claim_leading_ready_session_command(
        &self,
        session_id: &str,
        fence: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        let operation = TurnSeamOperation::Store(StoreOperation::ClaimLeadingSessionCommand);
        self.control
            .around(
                operation,
                self.inner
                    .claim_leading_ready_session_command(session_id, fence, owner),
            )
            .await
    }

    async fn claim_ready_queued_work(
        &self,
        session_id: &str,
        fence: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: QueuedWorkClaimBoundary,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<crate::QueuedWorkClaimOutcome, StoreError> {
        let operation = TurnSeamOperation::Store(StoreOperation::ClaimReadyQueuedWork {
            boundary: format!("{boundary:?}").to_ascii_lowercase(),
        });
        self.control
            .around(
                operation,
                self.inner
                    .claim_ready_queued_work(session_id, fence, owner, boundary, policy),
            )
            .await
    }

    async fn claim_checkpoint_work(
        &self,
        session_id: &str,
        fence: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &crate::TurnId,
        checkpoint: CheckpointKind,
        max_inputs: usize,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<(Option<TurnInputClaim>, Option<QueuedWorkClaim>), StoreError> {
        let operation = TurnSeamOperation::Store(StoreOperation::ClaimCheckpointWork {
            checkpoint: format!("{checkpoint:?}").to_ascii_lowercase(),
        });
        self.control
            .around(
                operation,
                self.inner.claim_checkpoint_work(
                    session_id, fence, owner, turn_id, checkpoint, max_inputs, policy,
                ),
            )
            .await
    }

    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        session_id: &str,
        fence: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: QueuedWorkClaimBoundary,
        ids: &[String],
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<crate::SelectedQueuedWorkClaimOutcome, StoreError> {
        let operation = TurnSeamOperation::Store(StoreOperation::ClaimSelectedQueuedWork {
            boundary: format!("{boundary:?}").to_ascii_lowercase(),
        });
        self.control
            .around(
                operation,
                self.inner.claim_ready_queued_work_by_batch_ids(
                    session_id, fence, owner, boundary, ids, policy,
                ),
            )
            .await
    }
}
struct SeamProvider {
    inner: Box<dyn Provider>,
    control: SeamControl,
}

impl Clone for SeamProvider {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone_boxed(),
            control: self.control.clone(),
        }
    }
}

impl std::fmt::Debug for SeamProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SeamProvider")
            .field("inner", &self.inner)
            .finish()
    }
}

fn provider_operation(request: &crate::LlmRequest) -> ProviderOperation {
    let after_tool = request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .any(|block| matches!(block, crate::llm::types::LlmContentBlock::ToolResult { .. }));
    if after_tool {
        ProviderOperation::AfterToolRequest
    } else {
        ProviderOperation::InitialRequest
    }
}

#[async_trait::async_trait]
impl Provider for SeamProvider {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
    fn route_identity(&self, model: &str) -> crate::ProviderRouteIdentity {
        self.inner.route_identity(model)
    }
    fn options(&self) -> crate::ProviderOptions {
        self.inner.options()
    }
    fn set_options(&mut self, options: crate::ProviderOptions) {
        self.inner.set_options(options);
    }
    fn serialize_config(&self) -> serde_json::Value {
        self.inner.serialize_config()
    }

    async fn complete(
        &mut self,
        request: crate::LlmRequest,
    ) -> Result<crate::LlmResponse, crate::llm::transport::LlmTransportError> {
        let operation = TurnSeamOperation::Provider(provider_operation(&request));
        self.control
            .around(operation, self.inner.complete(request))
            .await
    }

    fn requires_streaming(&self) -> bool {
        self.inner.requires_streaming()
    }
    async fn close(&self) -> Result<(), crate::llm::transport::LlmTransportError> {
        self.inner.close().await
    }
    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

#[derive(Clone)]
struct ScriptedProvider {
    control: SeamControl,
}

impl std::fmt::Debug for ScriptedProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ScriptedProvider")
    }
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    fn kind(&self) -> &'static str {
        "turn-crash-script"
    }
    fn route_identity(&self, model: &str) -> crate::ProviderRouteIdentity {
        crate::ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }
    fn options(&self) -> crate::ProviderOptions {
        crate::ProviderOptions::default()
    }
    fn set_options(&mut self, _options: crate::ProviderOptions) {}
    fn serialize_config(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    async fn complete(
        &mut self,
        request: crate::LlmRequest,
    ) -> Result<crate::LlmResponse, crate::llm::transport::LlmTransportError> {
        let operation = provider_operation(&request);
        let midpoint = match operation {
            ProviderOperation::InitialRequest => ProviderOperation::InitialMidStream,
            ProviderOperation::AfterToolRequest => ProviderOperation::AfterToolMidStream,
            ProviderOperation::InitialMidStream | ProviderOperation::AfterToolMidStream => {
                unreachable!()
            }
        };
        if let Some(events) = &request.stream_events {
            events.send(crate::llm::types::LlmStreamEvent::Delta(
                "trace".to_string(),
            ));
        }
        let midpoint = TurnSeamOperation::Provider(midpoint);
        self.control.record(midpoint.clone());
        if self
            .control
            .matches(&midpoint, CrashPlacement::ProviderMidStream)
        {
            self.control.stop_here().await;
        }
        if operation == ProviderOperation::InitialRequest {
            self.control
                .wait_until_completed(&TurnSeamOperation::Store(
                    StoreOperation::RenewSessionExecutionLease,
                ))
                .await;
            if self.control.renewals_are_starved() {
                // Hold the model response past the renewed lease's TTL. With
                // the renewal task starved above, the wall-clock fence lapses
                // before the turn reaches its `AfterWork` checkpoint.
                tokio::time::sleep(RECOVERY_TTL + RECOVERY_RENEW).await;
            }
        }
        Ok(match operation {
            ProviderOperation::InitialRequest => crate::LlmResponse {
                parts: vec![crate::LlmOutputPart::ToolCall {
                    call_id: "trace-tool-call".to_string(),
                    tool_name: "trace_effect".to_string(),
                    input_json: "{}".to_string(),
                    replay: None,
                }],
                ..Default::default()
            },
            ProviderOperation::AfterToolRequest => crate::LlmResponse {
                full_text: "trace turn complete".to_string(),
                parts: vec![crate::LlmOutputPart::Text {
                    text: "trace turn complete".to_string(),
                    response_meta: None,
                }],
                ..Default::default()
            },
            ProviderOperation::InitialMidStream | ProviderOperation::AfterToolMidStream => {
                unreachable!()
            }
        })
    }

    fn requires_streaming(&self) -> bool {
        true
    }
    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

#[derive(Clone)]
struct SeamEffectController {
    inner: Arc<dyn RuntimeEffectController>,
    control: SeamControl,
    executions: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for SeamEffectController {
    fn replay_ownership(&self) -> crate::EffectReplayOwnership {
        self.inner.replay_ownership()
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        self.inner.allows_process_lifetime_completion_keys()
    }

    async fn await_event_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
    ) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &crate::AwaitEventKey,
        resolution: crate::Resolution,
    ) -> Result<crate::ResolveOutcome, crate::RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &crate::AwaitEventKey,
    ) -> Result<Option<crate::Resolution>, crate::RuntimeError> {
        self.inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &crate::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<crate::Resolution, crate::RuntimeError> {
        self.inner.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), crate::RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), crate::RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for SeamEffectController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let operation = match &envelope.command {
            crate::RuntimeEffectCommand::ToolAttempt { call, .. } => Some((
                EffectOperation::ToolAttempt {
                    name: call.tool_name.clone(),
                },
                true,
            )),
            crate::RuntimeEffectCommand::ToolBatch { batch } => batch.calls.first().map(|call| {
                (
                    EffectOperation::ToolBatch {
                        name: call.call.tool_name.clone(),
                    },
                    false,
                )
            }),
            _ => None,
        };
        let Some((operation, counts_external_execution)) = operation else {
            return self.inner.execute_effect(envelope, executor).await;
        };
        let operation = TurnSeamOperation::Effect(operation);
        if !counts_external_execution {
            return self
                .control
                .around(operation, self.inner.execute_effect(envelope, executor))
                .await;
        }
        let executions = Arc::clone(&self.executions);
        let wrapped = RuntimeEffectLocalExecutor::testing(move |envelope| {
            let executions = Arc::clone(&executions);
            async move {
                executions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                executor.execute(envelope).await
            }
        });
        self.control
            .around(operation, self.inner.execute_effect(envelope, wrapped))
            .await
    }
}

#[derive(Clone)]
struct CrashAfterCheckpointExecutionController {
    inner: Arc<dyn RuntimeEffectController>,
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for CrashAfterCheckpointExecutionController {
    fn replay_ownership(&self) -> crate::EffectReplayOwnership {
        self.inner.replay_ownership()
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        self.inner.allows_process_lifetime_completion_keys()
    }

    async fn await_event_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
    ) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &crate::AwaitEventKey,
        resolution: crate::Resolution,
    ) -> Result<crate::ResolveOutcome, crate::RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &crate::AwaitEventKey,
    ) -> Result<Option<crate::Resolution>, crate::RuntimeError> {
        self.inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &crate::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<crate::Resolution, crate::RuntimeError> {
        self.inner.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), crate::RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<(), crate::RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for CrashAfterCheckpointExecutionController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if !matches!(
            &envelope.command,
            crate::RuntimeEffectCommand::Checkpoint {
                // The recorded AfterWork outcome supplies predecessor
                // authority. Crashing after BeforeCompletion executes forces
                // recovery to reclaim and journal its replacement authority.
                checkpoint: crate::CheckpointKind::BeforeCompletion,
            }
        ) {
            return self.inner.execute_effect(envelope, executor).await;
        }
        let crash_after_execution =
            RuntimeEffectLocalExecutor::testing(move |envelope| async move {
                let _outcome = executor.execute(envelope).await;
                std::process::exit(86);
            });
        self.inner
            .execute_effect(envelope, crash_after_execution)
            .await
    }
}

#[derive(Clone, Debug, Default)]
struct TraceTool {
    marker: Option<std::path::PathBuf>,
    control: SeamControl,
}

fn trace_tool_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:trace_effect",
        "trace_effect",
        "Execute the trace-derived conformance effect",
        serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        serde_json::json!({"type":"object"}),
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for TraceTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![trace_tool_definition().manifest()]
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == "trace_effect").then(|| Arc::new(trace_tool_definition().contract()))
    }
    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolOutcome {
        if let Some(marker) = &self.marker {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(marker)
                .expect("open level-2 external-effect marker");
            writeln!(file, "executed").expect("append level-2 external-effect marker");
            file.flush().expect("flush level-2 external-effect marker");
        }
        let operation = TurnSeamOperation::Effect(EffectOperation::ToolAttempt {
            name: "trace_effect".to_string(),
        });
        if self
            .control
            .matches(&operation, CrashPlacement::AfterExternalEffectBeforeOutcome)
        {
            self.control.stop_here().await;
        }
        crate::ToolOutcome::ok(serde_json::json!({"effect":"executed"}))
    }
}

fn recovery_timings() -> crate::LeaseTimings {
    crate::LeaseTimings::new(RECOVERY_TTL, RECOVERY_RENEW)
        .expect("300ms TTL / 100ms renew satisfies ttl >= 3x renew")
}

fn nominal_recovery_timings() -> crate::LeaseTimings {
    // The scripted provider deliberately awaits the first completed renewal.
    // Keep that nominal successor turn's lease window independent from the
    // short TTL used by the explicit starved-renewal case: a loaded runner may
    // delay the renewal task past 300ms without changing the awaited condition.
    crate::LeaseTimings::new(NOMINAL_RECOVERY_TTL, RECOVERY_RENEW)
        .expect("5s TTL / 100ms renew tolerates the nominal renewal barrier")
}

fn runtime_policy() -> crate::SessionPolicy {
    crate::SessionPolicy {
        provider_id: "turn-crash-script".to_string(),
        model: crate::ModelSpec::builder("turn-crash-model")
            .context_window_tokens(16_000)
            .build()
            .expect("valid conformance model"),
        ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded)
    }
}

fn provider_handle(control: SeamControl) -> ProviderHandle {
    let scripted: Box<dyn Provider> = Box::new(ScriptedProvider {
        control: control.clone(),
    });
    ProviderHandle::new(ProviderComponents::new(Box::new(SeamProvider {
        inner: scripted,
        control,
    })))
}

fn scoped_controller(
    controller: Arc<dyn RuntimeEffectController>,
    identity: &ReferenceIdentity,
) -> crate::ScopedEffectController<'static> {
    crate::ScopedEffectController::shared(
        controller,
        crate::ExecutionScope::turn(&identity.session_id, &identity.turn_id),
    )
    .expect("valid reference turn scope")
}

async fn build_runtime(
    store: Arc<dyn RuntimePersistence>,
    control: SeamControl,
    executions: Arc<std::sync::atomic::AtomicUsize>,
    identity: &ReferenceIdentity,
    trace_tool: TraceTool,
) -> crate::LashRuntime {
    build_runtime_with_lease_timings(
        store,
        control,
        executions,
        identity,
        trace_tool,
        recovery_timings(),
    )
    .await
}

async fn build_runtime_with_lease_timings(
    store: Arc<dyn RuntimePersistence>,
    control: SeamControl,
    executions: Arc<std::sync::atomic::AtomicUsize>,
    identity: &ReferenceIdentity,
    mut trace_tool: TraceTool,
    lease_timings: crate::LeaseTimings,
) -> crate::LashRuntime {
    super::bind_conformance_session(&store, &identity.session_id).await;
    let effect_controller: Arc<dyn RuntimeEffectController> = Arc::new(SeamEffectController {
        inner: Arc::new(crate::InlineRuntimeEffectController::default()),
        control: control.clone(),
        executions,
    });
    let mut host = crate::RuntimeHostConfig::new(
        Arc::new(crate::InlineEffectHost::new(Arc::clone(&effect_controller))),
        Arc::new(crate::InMemoryAttachmentStore::new()),
        Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    )
    .with_lease_timings(lease_timings);
    trace_tool.control = control.clone();
    host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(provider_handle(control)));
    let mut plugin_factories = crate::testing::test_standard_protocol_factories();
    plugin_factories.push(Arc::new(StaticPluginFactory::new(
        "turn_crash_trace_tool",
        PluginSpec::new().with_tool_provider(Arc::new(trace_tool)),
    )));
    Box::pin(
        crate::LashRuntime::builder(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
            crate::testing::runtime_lease_owner(),
        )
        .with_session_id(&identity.session_id)
        .with_policy(runtime_policy())
        .with_runtime_host(host)
        .with_store(store)
        .with_plugin_factories(plugin_factories)
        .build(),
    )
    .await
    .expect("build reference runtime")
}

async fn seed_reference_ingress(
    store: &Arc<dyn RuntimePersistence>,
    identity: &ReferenceIdentity,
    scenario: &str,
) {
    super::bind_conformance_session(store, &identity.session_id).await;
    // FIG-1573: one scenario deliberately seeds no next-turn row, so that after
    // the crash a recovering drain claims no next-turn input and therefore
    // evaluates the drain-time orphan backstop - with the active-turn row still
    // pinned to the turn it is about to resume.
    if !scenario.starts_with("peer-reclaim-pinned-active-input-") {
        store
            .enqueue_pending_turn_input(PendingTurnInputDraft::new(
                &identity.session_id,
                crate::TurnInputIngress::NextTurn,
                crate::TurnInput::text("durable next-turn input"),
            ))
            .await
            .expect("seed next-turn input");
    }
    store
        .enqueue_pending_turn_input(PendingTurnInputDraft::new(
            &identity.session_id,
            crate::TurnInputIngress::active_turn(
                &identity.turn_id,
                crate::TurnInputCheckpointBoundary::AfterWork,
            ),
            crate::TurnInput::text("active checkpoint input"),
        ))
        .await
        .expect("seed active-turn input");
    store
        .enqueue_queued_work(
            QueuedWorkBatchDraft::new(
                &identity.session_id,
                crate::DeliveryPolicy::EarliestSafeBoundary,
                vec![crate::QueuedWorkPayload::agent_frame_task(
                    crate::session_graph::frame_node_id(&identity.session_id, "trace-frame"),
                    "trace-source",
                    None,
                )],
            )
            .with_source_key("trace-derived-queued-work"),
        )
        .await
        .expect("seed queued work");
}

async fn drive_turn(
    mut runtime: crate::LashRuntime,
    effect_controller: Arc<dyn RuntimeEffectController>,
    identity: &ReferenceIdentity,
) -> Result<Option<crate::AssembledTurn>, crate::RuntimeError> {
    runtime
        .stream_next_queued_work(crate::TurnOptions::new(
            tokio_util::sync::CancellationToken::new(),
            scoped_controller(effect_controller, identity),
        ))
        .await
        .map(crate::facade_support::QueuedTurnDrain::ran)
}

fn generated_points(trace: &[TurnSeamOperation]) -> Vec<TurnCrashPoint> {
    let mut points = Vec::new();
    for operation in trace {
        let placement = match operation {
            TurnSeamOperation::Provider(
                ProviderOperation::InitialMidStream | ProviderOperation::AfterToolMidStream,
            ) => CrashPlacement::ProviderMidStream,
            _ => CrashPlacement::Boundary,
        };
        points.push(TurnCrashPoint {
            operation: operation.clone(),
            placement,
        });
        if operation.durable_write() {
            points.push(TurnCrashPoint {
                operation: operation.clone(),
                placement: CrashPlacement::InsideCall,
            });
        }
        if matches!(operation, TurnSeamOperation::Effect(_)) {
            if matches!(
                operation,
                TurnSeamOperation::Effect(EffectOperation::ToolAttempt { .. })
            ) {
                points.push(TurnCrashPoint {
                    operation: operation.clone(),
                    placement: CrashPlacement::AfterExternalEffectBeforeOutcome,
                });
            }
            points.push(TurnCrashPoint {
                operation: operation.clone(),
                placement: CrashPlacement::InsideCall,
            });
        }
    }
    points
}

fn golden_trace() -> Vec<TurnSeamOperation> {
    serde_json::from_str(GOLDEN_TRACE).expect("committed turn crash trace is valid")
}

/// Return the committed trace-derived matrix and its hand-written outcomes.
fn reviewed_turn_crash_rulings() -> Vec<ReviewedTurnCrashRuling> {
    serde_json::from_str(OUTCOME_TABLE).expect("committed turn crash outcome table is valid")
}

fn turn_crash_matrix_outcomes() -> Vec<TurnCrashOutcome> {
    reviewed_turn_crash_rulings()
        .into_iter()
        .filter_map(|ruling| match ruling {
            ReviewedTurnCrashRuling::CrashPoint(outcome) => Some(outcome),
            ReviewedTurnCrashRuling::DurableRecovery(_) => None,
        })
        .collect()
}

fn durable_recovery_rulings() -> Vec<DurableRecoveryRuling> {
    reviewed_turn_crash_rulings()
        .into_iter()
        .filter_map(|ruling| match ruling {
            ReviewedTurnCrashRuling::CrashPoint(_) => None,
            ReviewedTurnCrashRuling::DurableRecovery(ruling) => Some(ruling),
        })
        .collect()
}

fn validate_outcome_table(
    generated: &[TurnCrashPoint],
    table: &[TurnCrashOutcome],
) -> Result<(), String> {
    let table_points = table
        .iter()
        .map(|entry| entry.point.clone())
        .collect::<Vec<_>>();
    if table_points != generated {
        return Err(format!(
            "outcome table must cover every generated level-1 point exactly in trace order\nexpected: {generated:#?}\nactual: {table_points:#?}"
        ));
    }

    let expected_level_2 = ColdProcessTurnAction::CRASH_ACTIONS
        .into_iter()
        .map(|action| action.point().expect("crash action has a point"))
        .collect::<Vec<_>>();
    let actual_level_2 = table
        .iter()
        .filter(|entry| entry.level_2.is_some())
        .map(|entry| entry.point.clone())
        .collect::<Vec<_>>();
    let same_set = actual_level_2.len() == expected_level_2.len()
        && expected_level_2
            .iter()
            .all(|point| actual_level_2.contains(point));
    if !same_set {
        return Err(format!(
            "outcome table level-2 point set must match the cold-process actions\nexpected: {expected_level_2:#?}\nactual: {actual_level_2:#?}"
        ));
    }

    for entry in table.iter().filter(|entry| entry.level_2.is_some()) {
        let expectation = entry.level_2.as_ref().expect("filtered level-2 row");
        match (&expectation.exact, &expectation.known_defect) {
            (Some(exact), None) => {
                let observed = exact.exact().ok_or_else(|| {
                    format!(
                        "level-2 exact expectation must specify terminal, pending_inputs, and queued_work: {:?}",
                        entry.point
                    )
                })?;
                if observed != DurableEndState::CORRECT {
                    return Err(format!(
                        "ordinary level-2 expectation must assert the correct durable end state exactly: {:?}",
                        entry.point
                    ));
                }
            }
            (None, Some(known_defect)) => {
                if !is_ticket_id(&known_defect.ticket) {
                    return Err(format!(
                        "known-defect expectation requires a ticket id: {:?}",
                        entry.point
                    ));
                }
                let defective = known_defect.expected_defective.exact().ok_or_else(|| {
                    format!(
                        "known-defect expectation must specify terminal, pending_inputs, and queued_work exactly: {:?}",
                        entry.point
                    )
                })?;
                if defective == DurableEndState::CORRECT {
                    return Err(format!(
                        "known-defect expectation must differ from the correct durable end state: {:?}",
                        entry.point
                    ));
                }
                let correct_summary = DurableEndState::CORRECT.summary().replace(' ', ", ");
                if !entry.outcome.contains(&known_defect.ticket)
                    || !entry.outcome.contains(&correct_summary)
                {
                    return Err(format!(
                        "known-defect outcome prose must name {} and the correct end state `{correct_summary}`: {:?}",
                        known_defect.ticket, entry.point
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "level-2 expectation must contain exactly one of `exact` or `known_defect`: {:?}",
                    entry.point
                ));
            }
        }
    }
    Ok(())
}

fn validate_durable_recovery_rulings(rulings: &[DurableRecoveryRuling]) -> Result<(), String> {
    const EXPECTED_SCENARIOS: [&str; 4] = [
        "active_turn_input_pinned_to_recovered_turn",
        "checkpoint_execute_finalize",
        "checkpoint_replacement_double_crash",
        "peer_reclaim",
    ];
    let actual = rulings
        .iter()
        .map(|ruling| ruling.scenario.as_str())
        .collect::<Vec<_>>();
    if actual.len() != EXPECTED_SCENARIOS.len()
        || !EXPECTED_SCENARIOS
            .iter()
            .all(|expected| actual.contains(expected))
    {
        return Err(format!(
            "reviewed durable recovery scenarios must be exactly {EXPECTED_SCENARIOS:?}; got {actual:?}"
        ));
    }
    for ruling in rulings {
        if ruling.outcome.trim().is_empty() {
            return Err(format!(
                "durable recovery scenario `{}` must explain its ruling",
                ruling.scenario
            ));
        }
        if ruling.exact.exact().is_none() {
            return Err(format!(
                "durable recovery scenario `{}` must pin an exact end state",
                ruling.scenario
            ));
        }
    }
    Ok(())
}

fn is_ticket_id(ticket: &str) -> bool {
    let Some((project, number)) = ticket.split_once('-') else {
        return false;
    };
    !project.is_empty()
        && project
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        && !number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit())
}

/// Re-record the reference turn and fail if its live seam traffic drifts from
/// the committed golden trace or the outcome table omits a generated point.
pub(crate) async fn turn_crash_trace_drift_check<F>(make: F)
where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
{
    let control = SeamControl::default();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let raw = make("trace-drift");
    let identity = ReferenceIdentity::for_scenario("trace-drift");
    seed_reference_ingress(&raw, &identity, "trace-drift").await;
    let decorated = SeamStore::wrap(raw, control.clone());
    let runtime = Box::pin(build_runtime_with_lease_timings(
        decorated,
        control.clone(),
        Arc::clone(&executions),
        &identity,
        TraceTool::default(),
        nominal_recovery_timings(),
    ))
    .await;
    control.clear();
    // The golden trace below is an exact ordering; pin the one seam whose timing
    // is owned by a background timer rather than by the turn.
    control.pin_renewal_after_provider();
    let effect_controller: Arc<dyn RuntimeEffectController> = Arc::new(SeamEffectController {
        inner: Arc::new(crate::InlineRuntimeEffectController::default()),
        control: control.clone(),
        executions,
    });
    let turn = drive_turn(runtime, effect_controller, &identity)
        .await
        .expect("reference turn succeeds")
        .expect("reference ingress produces a turn");
    assert_eq!(turn.assistant_output.safe_text, "trace turn complete");
    assert_eq!(
        control.trace(),
        golden_trace(),
        "real turn seam trace drifted"
    );
    let generated = generated_points(&golden_trace());
    let table = turn_crash_matrix_outcomes();
    validate_outcome_table(&generated, &table)
        .unwrap_or_else(|error| panic!("invalid turn crash outcome table: {error}"));
    validate_durable_recovery_rulings(&durable_recovery_rulings())
        .unwrap_or_else(|error| panic!("invalid durable recovery rulings: {error}"));
}

async fn wait_for_recovery_lease<F>(
    make: &F,
    scenario: &str,
    point: &TurnCrashPoint,
    predecessor_claimed: bool,
) where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
{
    let identity = ReferenceIdentity::for_scenario(scenario);
    let owner = LeaseOwnerIdentity::opaque("recovery-probe", format!("{scenario}:probe"));
    let store = make(scenario);
    super::bind_conformance_session(&store, &identity.session_id).await;
    tokio::time::timeout(RECOVERY_TIMEOUT, async {
        loop {
            let outcome = store
                .try_claim_session_execution_lease(
                    &identity.session_id,
                    &owner,
                    "wait-for-recovery-lease-executor",
                    recovery_timings().ttl_ms(),
                )
                .await
                .expect("probe recovery lease");
            if let Some(acquisition) = outcome.acquisition() {
                match (predecessor_claimed, acquisition.displaced.as_ref()) {
                    (true, Some(displaced)) => {
                        assert_eq!(displaced.owner.owner_id, "lash-core-test-worker");
                    }
                    (false, None) => {}
                    (true, None) => panic!(
                        "crash recovery must displace the lapsed predecessor executor: {scenario} {point:?}"
                    ),
                    (false, Some(displaced)) => {
                        panic!("claim-boundary crash cannot have a predecessor, got {displaced:?}")
                    }
                }
                let lease = acquisition.lease;
                store
                    .release_session_execution_lease(&lease.completion())
                    .await
                    .expect("release recovery probe");
                break;
            }
            tokio::time::sleep(recovery_timings().renew_interval()).await;
        }
    })
    .await
    .expect("crashed turn lease becomes reclaimable by polling");
}

fn point_key(point: &TurnCrashPoint) -> String {
    let encoded = serde_json::to_vec(point).expect("serialize crash point");
    let digest = encoded.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    format!("matrix-{digest:016x}")
}

/// Effect executions contributed by one drain turn.
///
/// The scripted provider answers with a tool call only while the request has no
/// tool result in it. A drain turn replays the recovered turn's tool result in
/// history, so it is a single model call with a terminal response: it adds one
/// assistant output and no external effect, which keeps every ruled effect
/// count in the outcome table exact.
const DRAIN_TURN_EFFECT_EXECUTIONS: usize = 0;

/// Residual pending inputs a recovered turn is allowed to leave behind.
///
/// The only tolerated residue is a row deferred to the next turn: when the
/// session-execution lease lapses by wall clock, `claim_checkpoint_work` raises
/// [`StoreError::SessionExecutionLeaseExpired`], the runtime skips the advisory
/// checkpoint claim, and the commit defers the undelivered active-turn input to
/// the next turn instead of dropping or double-applying it. That deferral is a
/// designed outcome, so the matrix drains it with one more turn and holds the
/// exactly-once law on the drained input rather than on the intermediate row.
/// Any other residual state is a settlement defect and fails here.
fn deferred_to_next_turn(pending_inputs: &[PendingTurnInput]) -> bool {
    !pending_inputs.is_empty()
        && pending_inputs
            .iter()
            .all(|input| input.state.is_next_turn_pending())
}

/// The reference turn's committed user text for one pending input row.
fn pending_input_text(input: &PendingTurnInput) -> String {
    input
        .input
        .items
        .iter()
        .map(|item| match item {
            crate::InputItem::Text { text } => text.clone(),
            other => panic!("reference ingress only seeds text inputs, got {other:?}"),
        })
        .collect()
}

/// Run the trace-generated level-1 crash matrix against one persistence
/// backend. The factory must return fresh outer handles over the substrate
/// selected by its semantic scenario key.
///
/// Every generated crash point runs under a nominal lease-renewal task. The
/// matrix then replays the renewal-boundary point once more with the renewal
/// task deterministically starved, which pins the advisory checkpoint-skip
/// path (lease lapsed by wall clock) that a loaded runner would otherwise
/// reach only by luck.
pub async fn turn_crash_matrix_level_1<F>(make: F)
where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
{
    Box::pin(turn_crash_trace_drift_check(&make)).await;
    for entry in turn_crash_matrix_outcomes() {
        let scenario = point_key(&entry.point);
        Box::pin(run_crash_matrix_case(
            &make,
            &entry,
            &scenario,
            RenewalPressure::Nominal,
        ))
        .await;
    }
    let renewal_boundary = turn_crash_matrix_outcomes()
        .into_iter()
        .find(|entry| {
            entry.point
                == TurnCrashPoint {
                    operation: TurnSeamOperation::Store(StoreOperation::RenewSessionExecutionLease),
                    placement: CrashPlacement::Boundary,
                }
        })
        .expect("generated matrix contains the renewal-boundary crash point");
    let starved_scenario = format!("{}:starved-renewal", point_key(&renewal_boundary.point));
    Box::pin(run_crash_matrix_case(
        &make,
        &renewal_boundary,
        &starved_scenario,
        RenewalPressure::Starved,
    ))
    .await;
}

/// Crash one scripted turn at `entry`'s point, recover it with a successor
/// turn under `pressure`, and assert the ruled durable end state.
async fn run_crash_matrix_case<F>(
    make: &F,
    entry: &TurnCrashOutcome,
    scenario: &str,
    pressure: RenewalPressure,
) where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
{
    let identity = ReferenceIdentity::for_scenario(scenario);
    let raw = make(scenario);
    seed_reference_ingress(&raw, &identity, scenario).await;
    let control = SeamControl::default();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let decorated = SeamStore::wrap(raw, control.clone());
    let runtime = Box::pin(build_runtime(
        decorated,
        control.clone(),
        Arc::clone(&executions),
        &identity,
        TraceTool::default(),
    ))
    .await;
    control.arm(entry.point.clone());
    let effect_controller: Arc<dyn RuntimeEffectController> = Arc::new(SeamEffectController {
        inner: Arc::new(crate::InlineRuntimeEffectController::default()),
        control: control.clone(),
        executions: Arc::clone(&executions),
    });
    let task_identity = identity.clone();
    let task =
        crate::task::spawn(
            async move { drive_turn(runtime, effect_controller, &task_identity).await },
        );
    control.wait_for_hit().await;
    control.simulate_process_crash();
    task.abort();
    let _ = task.await;

    let predecessor_claimed = !matches!(
        (&entry.point.operation, entry.point.placement),
        (
            TurnSeamOperation::Store(StoreOperation::ClaimSessionExecutionLease),
            CrashPlacement::Boundary
        ) | (
            TurnSeamOperation::Store(StoreOperation::CommitFinalHead {
                releases_lease: true,
                ..
            }),
            CrashPlacement::InsideCall
        )
    );
    wait_for_recovery_lease(make, scenario, &entry.point, predecessor_claimed).await;
    let successor_control = SeamControl::default();
    let successor_store = SeamStore::wrap(make(scenario), successor_control.clone());
    let successor_timings = match pressure {
        RenewalPressure::Nominal => nominal_recovery_timings(),
        RenewalPressure::Starved => recovery_timings(),
    };
    let successor = Box::pin(build_runtime_with_lease_timings(
        Arc::clone(&successor_store),
        successor_control.clone(),
        Arc::clone(&executions),
        &identity,
        TraceTool::default(),
        successor_timings,
    ))
    .await;
    successor_control.clear();
    if pressure == RenewalPressure::Starved {
        successor_control.starve_renewals();
    }
    let successor_effect_controller: Arc<dyn RuntimeEffectController> =
        Arc::new(SeamEffectController {
            inner: Arc::new(crate::InlineRuntimeEffectController::default()),
            control: successor_control,
            executions: Arc::clone(&executions),
        });
    let _ = drive_turn(successor, successor_effect_controller, &identity)
        .await
        .unwrap_or_else(|error| panic!("successor failed for {scenario} ({entry:?}): {error}"));

    let reader = make(scenario);
    super::bind_conformance_session(&reader, &identity.session_id).await;
    let recovered_pending = reader
        .list_pending_turn_inputs(&identity.session_id)
        .await
        .expect("list pending inputs");
    // The recovered turn may legitimately leave one input deferred to the next
    // turn: a lapsed lease makes the checkpoint claim advisory, so the input is
    // carried forward rather than delivered. Drain it with one more turn and
    // hold the exactly-once law on the drained input below.
    if pressure == RenewalPressure::Starved {
        assert_eq!(
            recovered_pending
                .iter()
                .map(pending_input_text)
                .collect::<Vec<_>>(),
            vec!["active checkpoint input".to_string()],
            "{scenario}: a starved renewal must lapse the lease and defer the undelivered \
             active-turn input, or this case covers nothing; pending={recovered_pending:?}"
        );
    }
    let deferred_texts: Vec<String> = if recovered_pending.is_empty() {
        Vec::new()
    } else {
        assert!(
            deferred_to_next_turn(&recovered_pending),
            "{scenario} ({entry:?}): all input claims settle exactly once or defer to the next \
             turn; pending={recovered_pending:?}"
        );
        let texts: Vec<String> = recovered_pending.iter().map(pending_input_text).collect();
        // Deferral is tolerated only for the demoted active-turn input. A
        // seeded next-turn input showing up here would mean the recovered turn
        // failed to deliver work it was never blocked on, which the state-only
        // check above cannot tell apart from the designed deferral.
        assert!(
            texts
                .iter()
                .all(|text| text.as_str() == "active checkpoint input"),
            "{scenario} ({entry:?}): only the demoted active-turn input may defer to the next \
             turn; pending={recovered_pending:?}"
        );
        texts
    };
    let drain_turns = usize::from(!deferred_texts.is_empty());
    if drain_turns == 1 {
        Box::pin(drive_drain_turn(make, scenario, &identity, &executions)).await;
    }

    let state = crate::load_persisted_session_state(reader.as_ref())
        .await
        .expect("read recovered state")
        .expect("recovered turn commits state");
    let read_model = state.session_graph.read_model();
    let part_count = |content: &str| {
        read_model
            .messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter(|part| part.content == content)
            .count()
    };
    assert_eq!(
        part_count("trace turn complete"),
        1 + drain_turns,
        "{scenario} ({entry:?}): recovery must expose one terminal assistant output per turn"
    );
    for text in &deferred_texts {
        assert_eq!(
            part_count(text),
            1,
            "{scenario} ({entry:?}): the deferred input is delivered exactly once by the next turn"
        );
    }
    let pending_inputs = reader
        .list_pending_turn_inputs(&identity.session_id)
        .await
        .expect("list pending inputs");
    assert!(
        pending_inputs.is_empty(),
        "{scenario} ({entry:?}): all input claims settle exactly once; pending={pending_inputs:?}"
    );
    assert!(
        reader
            .list_queued_work(&identity.session_id)
            .await
            .expect("list queued work")
            .is_empty(),
        "{scenario} ({entry:?}): queued-work claim settles exactly once"
    );

    let effect_count = executions.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        effect_count,
        entry.effect_executions_l1 + drain_turns * DRAIN_TURN_EFFECT_EXECUTIONS,
        "{scenario}: {}",
        entry.outcome
    );
}

/// Drive one further clean turn to absorb inputs the recovered turn deferred to
/// the next turn.
async fn drive_drain_turn<F>(
    make: &F,
    scenario: &str,
    identity: &ReferenceIdentity,
    executions: &Arc<std::sync::atomic::AtomicUsize>,
) where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
{
    // The drain turn is a new turn, not a recovery of the crashed one, so it
    // gets its own turn identity: reusing the recovered turn's id would collide
    // with the history nodes that turn already committed.
    let identity = ReferenceIdentity {
        session_id: identity.session_id.clone(),
        turn_id: format!("{}:drain", identity.turn_id),
    };
    let identity = &identity;
    let control = SeamControl::default();
    let store = SeamStore::wrap(make(scenario), control.clone());
    let runtime = Box::pin(build_runtime_with_lease_timings(
        store,
        control.clone(),
        Arc::clone(executions),
        identity,
        TraceTool::default(),
        nominal_recovery_timings(),
    ))
    .await;
    control.clear();
    let effect_controller: Arc<dyn RuntimeEffectController> = Arc::new(SeamEffectController {
        inner: Arc::new(crate::InlineRuntimeEffectController::default()),
        control,
        executions: Arc::clone(executions),
    });
    let _ = drive_turn(runtime, effect_controller, identity)
        .await
        .unwrap_or_else(|error| panic!("drain turn failed for {scenario}: {error}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install_known_defect_fixture(table: &mut [TurnCrashOutcome]) -> &mut KnownDefectExpectation {
        let entry = table
            .iter_mut()
            .find(|entry| entry.level_2.is_some())
            .expect("level-2 row");
        let expectation = entry.level_2.as_mut().expect("level-2 expectation");
        expectation.exact = None;
        expectation.known_defect = Some(KnownDefectExpectation {
            ticket: "FIG-999".to_string(),
            expected_defective: DurableEndStateExpectation {
                terminal: Some(1),
                pending_inputs: Some(0),
                queued_work: Some(1),
            },
        });
        entry.outcome = "KNOWN-DEFECT FIG-999; correct durable end state terminal=1, pending_inputs=0, queued_work=0".to_string();
        expectation
            .known_defect
            .as_mut()
            .expect("installed known-defect fixture")
    }

    #[test]
    fn golden_trace_generates_exactly_the_reviewed_outcome_table() {
        let generated = generated_points(&golden_trace());
        let table = turn_crash_matrix_outcomes();
        validate_outcome_table(&generated, &table).expect("committed table is valid");
        validate_durable_recovery_rulings(&durable_recovery_rulings())
            .expect("committed durable recovery rulings are valid");
    }

    #[test]
    fn outcome_validation_rejects_a_dropped_level_1_point() {
        let generated = generated_points(&golden_trace());
        let mut table = turn_crash_matrix_outcomes();
        table.remove(4);
        assert!(
            validate_outcome_table(&generated, &table).is_err(),
            "removing any generated level-1 point must invalidate the oracle"
        );
    }

    #[test]
    fn outcome_validation_rejects_a_relocated_level_2_expectation() {
        let generated = generated_points(&golden_trace());
        let mut table = turn_crash_matrix_outcomes();
        let source = table
            .iter()
            .position(|entry| {
                entry.point
                    == ColdProcessTurnAction::ProviderInitialMidStream
                        .point()
                        .expect("crash point")
            })
            .expect("level-2 source row");
        let destination = table
            .iter()
            .position(|entry| {
                entry.point
                    == TurnCrashPoint {
                        operation: TurnSeamOperation::Store(StoreOperation::LoadSessionHeadMeta),
                        placement: CrashPlacement::Boundary,
                    }
            })
            .expect("level-1-only destination row");
        table[destination].level_2 = table[source].level_2.take();
        assert!(
            validate_outcome_table(&generated, &table).is_err(),
            "moving a level-2 expectation to a different point must invalidate the oracle"
        );
    }

    #[test]
    fn outcome_validation_rejects_a_known_defect_without_a_ticket() {
        let generated = generated_points(&golden_trace());
        let mut table = turn_crash_matrix_outcomes();
        assert!(
            validate_outcome_table(&generated, &table).is_ok(),
            "the synthetic defect test must start from a valid oracle"
        );
        let defect = install_known_defect_fixture(&mut table);
        defect.ticket.clear();
        assert!(
            validate_outcome_table(&generated, &table).is_err(),
            "a known defect without a ticket id must invalidate the oracle"
        );
    }

    #[test]
    fn outcome_validation_rejects_a_non_exact_known_defect() {
        let generated = generated_points(&golden_trace());
        let mut table = turn_crash_matrix_outcomes();
        assert!(
            validate_outcome_table(&generated, &table).is_ok(),
            "the synthetic defect test must start from a valid oracle"
        );
        let defect = install_known_defect_fixture(&mut table);
        defect.expected_defective.queued_work = None;
        assert!(
            validate_outcome_table(&generated, &table).is_err(),
            "a non-exact known-defect state must invalidate the oracle"
        );
    }

    #[test]
    fn generated_point_keys_are_unique() {
        let mut keys = std::collections::BTreeMap::new();
        for point in generated_points(&golden_trace()) {
            let key = point_key(&point);
            assert!(keys.insert(key, point).is_none());
        }
    }
}
