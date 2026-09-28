//! Trace-derived crash coverage for one real scripted runtime turn.
//!
//! This suite instruments only integrator-owned seams: [`RuntimePersistence`],
//! [`crate::Provider`], and [`RuntimeEffectController`]. The runtime turn loop
//! has no test hooks or failpoints (ADR 0044). A reference turn containing
//! next-turn input, queued work, active-turn input at `AfterWork`, a model tool
//! call, and a final model response produces the committed golden trace. The
//! crash matrix is generated from that trace: every operation has a boundary
//! crash, every durable write has an inside-call lost-response crash, and the
//! scripted provider contributes its own mid-stream crash points. Recovery
//! replays the recorded drive admission and fences stale claim settlement by
//! drive epoch and owner incarnation.
//!
//! Trace drift covers the operations explicitly decorated by this module.
//! Durable-store methods that [`SeamStore`] passes through undecorated are
//! outside that seam-coverage boundary until they are deliberately modeled.
//!
//! Beside the crash placements, [`turn_crash_matrix_error_return_fail_stop`]
//! places an error *return* — not a crash — at the tool-attempt seam
//! (FIG-3524). After an unretried storage error the
//! turn must stop: the fail-stop oracle asserts no durable commit, no tool
//! dispatch and no provider request follows, and that the typed store error
//! reaches the caller. Each placement's ruling is a reviewable row in
//! `turn_crash_outcomes.json`; a row pinning violations is a known-defect
//! entry under the same rule as the level-2 defect rulings.
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

use lash_core::testing::RuntimePersistenceTestClaimExt as _;
use lash_core::testing::TestTurnDrive as _;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use lash_sansio::sync::MutexExt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::plugin::{PluginSpec, StaticPluginFactory};
use crate::provider::{Provider, ProviderComponents, ProviderHandle};
use crate::store::{PersistedSessionRead, RuntimeCommit, RuntimeCommitReceipt};
use crate::{
    CheckpointKind, ClaimAuthority, LeaseOwnerIdentity, PendingTurnInputDraft, QueuedWorkClaim,
    QueuedWorkClaimBoundary, RuntimeEffectController, RuntimePersistence, SessionHeadMeta,
    StoreError, TurnInputClaim,
};

mod after_commit_redrive;
mod cancel_closure;
mod cold_process;
mod direct_acceptance;
mod error_return;
mod expectations;
mod held_turn_input;
mod invocation_effect_host;
mod layered_group_child;
mod recovery;
mod reference_turn;
mod seam_controllers;

use recovery::run_crash_matrix_case;
use reference_turn::ReferenceTurn;

pub use after_commit_redrive::turn_crash_after_commit_redrive_replays_the_committed_receipt;
pub use cancel_closure::turn_cancel_closure_recovers_from_a_crash_at_every_cut;
use cold_process::ColdProcessTurnAction;
pub use cold_process::{
    cold_process_durable_recovery_expectation, cold_process_real_turn_driver,
    cold_process_turn_cancel_actions, cold_process_turn_expectations, cold_process_turn_scope,
};
pub use direct_acceptance::direct_turn_acceptance_crash_after_store_commit_admits_one_row;
use error_return::{ErrorReturnPlacement, ErrorReturnRuling, ErrorReturnRulingEntry};
pub use error_return::{
    FailStopObservation, FailStopViolation, fail_stop_violations,
    turn_crash_matrix_error_return_fail_stop,
};
use expectations::{
    durable_recovery_rulings, error_return_rulings, turn_crash_matrix_outcomes,
    validate_durable_recovery_rulings, validate_error_return_rulings, validate_outcome_table,
};
pub use held_turn_input::held_turn_input_visibility_survives_claim_holder_crash;
use invocation_effect_host::InvocationEffectHost;
pub use layered_group_child::a_host_layer_observes_its_group_childrens_effects;
use pretty_assertions::assert_eq;
pub(crate) use seam_controllers::{
    CrashAfterCheckpointExecutionController, LawSeamHost, SeamLayer,
};

const GOLDEN_TRACE: &str = include_str!("turn_crash_trace.json");
const OUTCOME_TABLE: &str = include_str!("turn_crash_outcomes.json");
const RECOVERY_TTL: Duration = Duration::from_secs(3);
const RECOVERY_RENEW: Duration = Duration::from_millis(100);
const NOMINAL_RECOVERY_TTL: Duration = Duration::from_secs(5);
const CRASHED_TURN_TTL: Duration = Duration::from_secs(60);
const HIT_TIMEOUT: Duration = Duration::from_secs(60);
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
struct ReferenceIdentity {
    session_id: SessionId,
    turn_id: TurnId,
}

impl ReferenceIdentity {
    fn for_scenario(scenario: &str) -> Self {
        let session_id = SessionId::from(format!("trace-derived-real-turn:{scenario}"));
        let turn_id = TurnId::from(format!("{session_id}:turn"));
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
    TurnControl(TurnControlOperation),
}

impl TurnSeamOperation {
    fn durable_write(&self) -> bool {
        matches!(
            self,
            Self::Store(
                StoreOperation::AdmitRoot
                    | StoreOperation::ClaimNextTurnInputs
                    | StoreOperation::ClaimReadyQueuedWork { .. }
                    | StoreOperation::ClaimCheckpointWork { .. }
                    | StoreOperation::CommitFinalHead { .. }
                    | StoreOperation::AuthorizeTurnCancelClosure
                    | StoreOperation::ApplyTurnCancelEffectsAndConsume
            ) | Self::TurnControl(_)
        )
    }
}

/// Semantic store calls crossed by the reference turn.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum StoreOperation {
    LoadSession,
    LoadSessionHeadMeta,
    ClaimLeadingSessionCommand,
    UnfinishedRoot,
    AdmitRoot,
    ClaimNextTurnInputs,
    DeferOrphanedActiveTurnInputs,
    ClaimReadyQueuedWork {
        boundary: String,
    },
    ClaimCheckpointWork {
        checkpoint: String,
    },
    CommitFinalHead {
        settles_queue: bool,
        settles_turn_input: bool,
    },
    AuthorizeTurnCancelClosure,
    ApplyTurnCancelEffectsAndConsume,
}

/// Durable promise calls that close one authorized turn-cancellation decision.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum TurnControlOperation {
    ResolveBase,
    ResolveEscalation,
}

fn turn_control_resolution_operation(key: &crate::AwaitEventKey) -> Option<TurnSeamOperation> {
    match key.wait {
        crate::AwaitEventWaitIdentity::TurnCancelGate => Some(TurnSeamOperation::TurnControl(
            TurnControlOperation::ResolveBase,
        )),
        crate::AwaitEventWaitIdentity::TurnCancelEscalation => Some(
            TurnSeamOperation::TurnControl(TurnControlOperation::ResolveEscalation),
        ),
        _ => None,
    }
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
///
/// `GroupChild` is a tool-attempt envelope that arrived carrying group
/// membership — the same command under a different authority, which is the
/// state an attempt-only vocabulary cannot name (FIG-3429). The group
/// lifecycle calls are seam operations of their own: an open is where retained
/// membership is written, a settlement is where a journaled rank is consumed,
/// and a close is where the caller releases its losers.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum EffectOperation {
    ToolAttempt {
        name: String,
    },
    GroupChild {
        name: String,
    },
    GroupOpen {
        children: usize,
    },
    GroupSettle,
    GroupClose,
    /// A direct turn's journaled acceptance write (ADR 0069 §6). The reference
    /// turn is a drain and never accepts; the direct-turn acceptance scenario
    /// ([`direct_turn_acceptance_crash_after_store_commit_admits_one_row`])
    /// crashes here.
    AcceptTurnInput,
    /// The turn's start-gate cancel peek. It crosses the seam only when an
    /// error return is armed on it, so the golden trace does not carry it.
    StartGatePeek,
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct DurableEndState {
    terminal: usize,
    pending_inputs: usize,
    queued_work: usize,
}

/// An ordinary level-2 ruling: the recovered durable end state is exactly
/// [`DurableEndState::CORRECT`], so the triple is not restated per row.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Level2ExactExpectation {
    effect_executions: Level2EffectExecutions,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct KnownDefectExpectation {
    effect_executions: Level2EffectExecutions,
    ticket: String,
    expected_defective: DurableEndState,
}

/// Exactly one of the two level-2 end-state rulings; the externally tagged
/// representation rejects rows carrying both or neither.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Level2Expectation {
    Exact(Level2ExactExpectation),
    KnownDefect(KnownDefectExpectation),
}

impl Level2Expectation {
    fn effect_executions(&self) -> &Level2EffectExecutions {
        match self {
            Self::Exact(exact) => &exact.effect_executions,
            Self::KnownDefect(defect) => &defect.effect_executions,
        }
    }
}

/// Reviewable recovery ruling for one generated point.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct TurnCrashOutcome {
    point: TurnCrashPoint,
    outcome: String,
    #[serde(default)]
    level_2: Option<Level2Expectation>,
}

/// Reviewable durable end-state ruling for a composed level-2 trajectory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct DurableRecoveryRuling {
    scenario: String,
    outcome: String,
    exact: DurableEndState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
enum ReviewedTurnCrashRuling {
    CrashPoint(TurnCrashOutcome),
    DurableRecovery(DurableRecoveryRuling),
    ErrorReturn(ErrorReturnRulingEntry),
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
    armed: Option<TurnCrashPoint>,
    /// The armed error-return placement (FIG-3524), independent of `armed`:
    /// a crash point and an error return are never armed on the same run.
    error_return: Option<ErrorReturnPlacement>,
    /// Whether a tool attempt already consumed the armed error return.
    error_return_taken: bool,
    hit: bool,
    process_crashed: bool,
}

#[derive(Clone, Debug, Default)]
struct SeamControl {
    state: Arc<Mutex<SeamState>>,
    hit: Arc<tokio::sync::Notify>,
    /// Notified on every [`SeamControl::record`], so a seam can wait for another
    /// seam to appear in the trace rather than poll for it.
    recorded: Arc<tokio::sync::Notify>,
    /// Cancelled when the law's crash kills the process running the turn: an
    /// execution the engine runs apart from the turn's own (a group child's)
    /// dies with it ([`SeamControl::process_crash`]).
    process_crash: tokio_util::sync::CancellationToken,
}

impl SeamControl {
    fn record(&self, operation: TurnSeamOperation) {
        self.state.lock_recover().trace.push(operation);
        self.recorded.notify_one();
    }

    fn arm(&self, point: TurnCrashPoint) {
        let mut state = self.state.lock_recover();
        state.trace.clear();
        state.armed = Some(point);
        state.error_return = None;
        state.error_return_taken = false;
        state.hit = false;
        state.process_crashed = false;
    }

    fn clear(&self) {
        let mut state = self.state.lock_recover();
        state.trace.clear();
        state.armed = None;
        state.error_return = None;
        state.error_return_taken = false;
        state.hit = false;
        state.process_crashed = false;
    }

    /// Arm an error-return placement for the fail-stop sweep (FIG-3524).
    fn arm_error_return(&self, placement: ErrorReturnPlacement) {
        let mut state = self.state.lock_recover();
        state.trace.clear();
        state.armed = None;
        state.error_return = Some(placement);
        state.error_return_taken = false;
        state.hit = false;
        state.process_crashed = false;
    }

    /// The armed error-return placement, consumed by the first tool attempt
    /// that reaches it: a retry of the faulted attempt runs clean, as a retry
    /// after a real store blip does. An engine that retries a failed group
    /// child itself (Restate) re-enters the seam; the law reads that
    /// re-entry as the retry.
    fn take_tool_attempt_error_return(&self) -> Option<ErrorReturnPlacement> {
        let mut state = self.state.lock_recover();
        if state.error_return_taken {
            return None;
        }
        let placement = state.error_return?;
        state.error_return_taken = true;
        Some(placement)
    }

    fn simulate_process_crash(&self) {
        self.state.lock_recover().process_crashed = true;
        self.process_crash.cancel();
    }

    /// Resolves once the law's crash killed the process running this seam's
    /// turn.
    async fn process_crash(&self) {
        self.process_crash.cancelled().await;
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
        self.hit.notify_one();
        std::future::pending().await
    }

    async fn wait_for_hit(&self) {
        let armed = self.state.lock_recover().armed.clone();
        tokio::time::timeout(HIT_TIMEOUT, self.hit.notified())
            .await
            .unwrap_or_else(|_| panic!("armed semantic seam operation was not reached: {armed:?}"));
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
        output
    }

    /// `around` for a wait whose result is produced by work running beside
    /// it: a group settlement is awaited while its child runs, so recording
    /// the wait's entry would pin a race between the two. The boundary still
    /// stops before the wait; the operation is recorded when the settlement
    /// is returned, which is always after the child's own seam traffic.
    async fn around_completion<T, F>(&self, operation: TurnSeamOperation, future: F) -> T
    where
        F: Future<Output = T>,
    {
        if self.matches(&operation, CrashPlacement::Boundary) {
            self.stop_here().await;
        }
        let output = future.await;
        self.record(operation.clone());
        if self.matches(&operation, CrashPlacement::InsideCall) {
            self.stop_here().await;
        }
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
    async fn unfinished_root(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<crate::store::UnfinishedRoot>, StoreError> {
        self.control
            .around(
                TurnSeamOperation::Store(StoreOperation::UnfinishedRoot),
                self.inner.unfinished_root(session_id),
            )
            .await
    }

    async fn admit_root(
        &self,
        request: &crate::store::AdmitRootRequest,
    ) -> Result<Option<crate::store::RootAdmission>, StoreError> {
        self.control
            .around(
                TurnSeamOperation::Store(StoreOperation::AdmitRoot),
                self.inner.admit_root(request),
            )
            .await
    }
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
        let operation = if commit.turn_cancel_closure_settlement.is_some() {
            TurnSeamOperation::Store(StoreOperation::ApplyTurnCancelEffectsAndConsume)
        } else {
            TurnSeamOperation::Store(StoreOperation::CommitFinalHead {
                settles_queue: !commit.completed_queue_claims.is_empty(),
                settles_turn_input: !commit.completed_turn_input_claims.is_empty(),
            })
        };
        self.control
            .around(operation, self.inner.commit_runtime_state(commit))
            .await
    }

    async fn authorize_turn_cancel_closure(
        &self,
        session_execution_lease: &ClaimAuthority,
        authorization: &crate::TurnCancelClosureAuthorization,
    ) -> Result<crate::TurnCancelClosureAuthorizationOutcome, StoreError> {
        self.control
            .around(
                TurnSeamOperation::Store(StoreOperation::AuthorizeTurnCancelClosure),
                self.inner
                    .authorize_turn_cancel_closure(session_execution_lease, authorization),
            )
            .await
    }

    async fn claim_next_turn_inputs(
        &self,
        session_id: &SessionId,
        fence: &ClaimAuthority,
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

    async fn orphaned_active_turn_ids(
        &self,
        session_id: &SessionId,
        session_execution_lease: &crate::ClaimAuthority,
        scope: crate::OrphanedTurnInputScope<'_>,
    ) -> Result<Vec<crate::TurnId>, StoreError> {
        let operation = TurnSeamOperation::Store(StoreOperation::DeferOrphanedActiveTurnInputs);
        self.control
            .around(
                operation,
                self.inner
                    .orphaned_active_turn_ids(session_id, session_execution_lease, scope),
            )
            .await
    }

    async fn repair_orphaned_active_turn_inputs(
        &self,
        session_id: &SessionId,
        session_execution_lease: &crate::ClaimAuthority,
        turn_id: &crate::TurnId,
        observed: &crate::TurnCancelIntentSnapshot,
        settlement: Option<&crate::TurnCancelClosureSettlement>,
    ) -> Result<crate::TurnCancelRepairResult, StoreError> {
        let operation = if settlement.is_some() {
            TurnSeamOperation::Store(StoreOperation::ApplyTurnCancelEffectsAndConsume)
        } else {
            TurnSeamOperation::Store(StoreOperation::DeferOrphanedActiveTurnInputs)
        };
        self.control
            .around(
                operation,
                self.inner.repair_orphaned_active_turn_inputs(
                    session_id,
                    session_execution_lease,
                    turn_id,
                    observed,
                    settlement,
                ),
            )
            .await
    }

    async fn claim_leading_ready_session_command(
        &self,
        session_id: &SessionId,
        fence: &ClaimAuthority,
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
        session_id: &SessionId,
        fence: &ClaimAuthority,
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
        session_id: &SessionId,
        fence: &ClaimAuthority,
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
    async fn reconcile_usage(
        &mut self,
        generation_id: &str,
    ) -> Result<
        Option<lash_core::provider::ReconciledUsage>,
        crate::llm::transport::LlmTransportError,
    > {
        self.inner.reconcile_usage(generation_id).await
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
            events.send(crate::llm::types::LlmStreamEvent::Delta {
                block: crate::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: "trace".to_string(),
            });
        }
        let midpoint = TurnSeamOperation::Provider(midpoint);
        self.control.record(midpoint.clone());
        if self
            .control
            .matches(&midpoint, CrashPlacement::ProviderMidStream)
        {
            self.control.stop_here().await;
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

#[derive(Clone, Debug, Default)]
struct TraceTool {
    marker: Option<std::path::PathBuf>,
    control: SeamControl,
    /// How many times the tool body ran: the external effect every tier runs
    /// in process, wherever its engine dispatches the tool child.
    executed: Arc<std::sync::atomic::AtomicUsize>,
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
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
        crate::ToolOutcome::ok(serde_json::json!({"effect":"executed"})).into()
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn recovery_timings() -> crate::LeaseTimings {
    crate::LeaseTimings::new(RECOVERY_TTL, RECOVERY_RENEW)
        .expect("3s TTL / 100ms renew satisfies ttl >= 3x renew")
}

/// Configuration shared with the runtime fixture for a turn about to crash.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn crashed_turn_timings() -> crate::LeaseTimings {
    crate::LeaseTimings::new(CRASHED_TURN_TTL, RECOVERY_RENEW)
        .expect("60s TTL / 100ms renew satisfies ttl >= 3x renew")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn nominal_recovery_timings() -> crate::LeaseTimings {
    crate::LeaseTimings::new(NOMINAL_RECOVERY_TTL, RECOVERY_RENEW)
        .expect("5s TTL / 100ms interval satisfies fixture configuration")
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn scoped_controller(
    controller: Arc<dyn RuntimeEffectController>,
    identity: &ReferenceIdentity,
) -> crate::ScopedEffectController<'static> {
    crate::ScopedEffectController::shared(
        controller,
        crate::AdmittedScope::queue_drain(&identity.session_id, identity.turn_id.as_str()),
    )
    .expect("valid reference turn scope")
}

/// The execution scope the reference turn runs under: the scope a
/// journaled invocation for that turn must be opened on.
fn reference_turn_scope(identity: &ReferenceIdentity) -> crate::ExecutionScope {
    crate::ExecutionScope::queue_drain(&identity.session_id, identity.turn_id.as_str())
}

async fn build_runtime(
    stores: Arc<dyn crate::StoreSet>,
    store: Arc<dyn RuntimePersistence>,
    control: SeamControl,
    effect_controller: Arc<dyn RuntimeEffectController>,
    identity: &ReferenceIdentity,
    trace_tool: TraceTool,
) -> crate::LashRuntime {
    build_runtime_with_lease_timings(
        stores,
        store,
        control,
        effect_controller,
        identity,
        trace_tool,
        crashed_turn_timings(),
    )
    .await
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_runtime_with_lease_timings(
    stores: Arc<dyn crate::StoreSet>,
    store: Arc<dyn RuntimePersistence>,
    control: SeamControl,
    effect_controller: Arc<dyn RuntimeEffectController>,
    identity: &ReferenceIdentity,
    trace_tool: TraceTool,
    lease_timings: crate::LeaseTimings,
) -> crate::LashRuntime {
    Box::pin(try_build_runtime_with_lease_timings(
        stores,
        store,
        control,
        effect_controller,
        identity,
        trace_tool,
        lease_timings,
    ))
    .await
    .expect("build reference runtime")
}

/// Build the reference runtime, returning the builder's refusal instead of
/// panicking on it: session admission runs inside `build`.
async fn try_build_runtime_with_lease_timings(
    stores: Arc<dyn crate::StoreSet>,
    store: Arc<dyn RuntimePersistence>,
    control: SeamControl,
    effect_controller: Arc<dyn RuntimeEffectController>,
    identity: &ReferenceIdentity,
    trace_tool: TraceTool,
    lease_timings: crate::LeaseTimings,
) -> Result<crate::LashRuntime, crate::SessionError> {
    assert!(
        effect_controller
            .await_event_authority_binding_id()
            .is_some(),
        "durable crash fixture identifies its promise authority"
    );
    let effect_host: Arc<dyn crate::EffectHost> = Arc::new(InvocationEffectHost {
        inner: Arc::clone(&effect_controller),
    });
    Box::pin(try_build_runtime_over_host(
        stores,
        store,
        control,
        effect_host,
        identity,
        trace_tool,
        lease_timings,
    ))
    .await
}

/// The reference runtime on the tier's own `host`, behind `seam`: the runtime
/// a runner-driven crash law builds inside its attempt. The turn itself runs
/// on the controller the tier's [`ConformanceTurnRunner`](crate::ConformanceTurnRunner)
/// lends, behind the same seam ([`SeamLayer::over_scoped`]), so the seam sees
/// the turn's effects and turn-control resolutions on every tier.
async fn try_build_runtime_on_host(
    stores: Arc<dyn crate::StoreSet>,
    store: Arc<dyn RuntimePersistence>,
    seam: &SeamLayer,
    host: LawSeamHost,
    identity: &ReferenceIdentity,
    trace_tool: TraceTool,
    lease_timings: crate::LeaseTimings,
) -> Result<crate::LashRuntime, crate::SessionError> {
    host.route_to(seam);
    Box::pin(try_build_runtime_over_host(
        stores,
        store,
        seam.control.clone(),
        host.host(),
        identity,
        trace_tool,
        lease_timings,
    ))
    .await
}

async fn try_build_runtime_over_host(
    stores: Arc<dyn crate::StoreSet>,
    store: Arc<dyn RuntimePersistence>,
    control: SeamControl,
    effect_host: Arc<dyn crate::EffectHost>,
    identity: &ReferenceIdentity,
    mut trace_tool: TraceTool,
    lease_timings: crate::LeaseTimings,
) -> Result<crate::LashRuntime, crate::SessionError> {
    super::bind_conformance_session(&store, &identity.session_id).await;
    let mut host = crate::LawBackend::over_stores(stores, effect_host)
        .host_config(
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
            host,
            crate::LeaseOwnerIdentity::opaque(
                "turn-crash-matrix",
                uuid::Uuid::new_v4().to_string(),
            ),
        )
        .with_session_id(&identity.session_id)
        .with_policy(runtime_policy())
        .with_store(store)
        .with_plugin_factories(plugin_factories)
        .build(),
    )
    .await
}

async fn seed_reference_ingress(
    store: &Arc<dyn RuntimePersistence>,
    identity: &ReferenceIdentity,
    scenario: &str,
) {
    seed_reference_ingress_as(store, identity, scenario, Some(&identity.turn_id)).await;
}

/// The reference ingress for a drain through the session drive: the
/// next-turn input carries the reference turn id as its host id, so the
/// drive admits it as the root of that id and the active-turn input and any
/// cancellation addressed to the turn reach the root (FIG-3600 ruling Q4).
async fn seed_reference_ingress_for_drive(
    store: &Arc<dyn RuntimePersistence>,
    identity: &ReferenceIdentity,
    scenario: &str,
) {
    seed_reference_ingress_as(store, identity, scenario, Some(&identity.turn_id)).await;
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn seed_reference_ingress_as(
    store: &Arc<dyn RuntimePersistence>,
    identity: &ReferenceIdentity,
    scenario: &str,
    root: Option<&TurnId>,
) {
    super::bind_conformance_session(store, &identity.session_id).await;
    // FIG-1573: one scenario deliberately seeds no next-turn row, so a recovering
    // drain evaluates the drain-time orphan backstop.
    if !scenario.starts_with("peer-reclaim-pinned-active-input-") {
        let draft = PendingTurnInputDraft::new(
            &identity.session_id,
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("durable next-turn input"),
        );
        let draft = match root {
            Some(root) => draft.with_source_key(root.as_str()),
            None => draft,
        };
        store
            .enqueue_pending_turn_input(draft)
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
        .enqueue_queued_work(crate::conformance::helpers::process_wake_work(
            &identity.session_id,
            "trace-derived-queued-work",
            1,
            "trace-source",
            crate::DeliveryPolicy::EarliestSafeBoundary,
        ))
        .await
        .expect("seed queued work");
}

async fn drive_turn(
    mut runtime: crate::LashRuntime,
    effect_controller: Arc<dyn RuntimeEffectController>,
    identity: &ReferenceIdentity,
) -> Result<Option<crate::AssembledTurn>, crate::RuntimeError> {
    Box::pin(
        runtime.drive_one_admitted_queued_root(crate::TurnOptions::new(
            tokio_util::sync::CancellationToken::new(),
            scoped_controller(effect_controller, identity),
        )),
    )
    .await
    .map(crate::facade_support::QueuedTurnDrain::ran)
}

/// Drain the reference turn through the session drive on the controller a
/// tier's runner lent it.
async fn drive_root_on(
    mut runtime: crate::LashRuntime,
    scoped: crate::ScopedEffectController<'_>,
) -> Result<Option<crate::AssembledTurn>, crate::RuntimeError> {
    Box::pin(
        runtime.drive_one_admitted_queued_root(crate::TurnOptions::new(
            tokio_util::sync::CancellationToken::new(),
            scoped,
        )),
    )
    .await
    .map(crate::facade_support::QueuedTurnDrain::ran)
}

/// Park the turn at `control`'s armed point and fire `crash` there: the crash
/// trigger a runner-driven law hands [`ConformanceTurnRunner::run_turn_until_crash`](crate::ConformanceTurnRunner::run_turn_until_crash).
/// The seam marks the process crashed before firing the runner's crash point.
fn crash_at_armed_point(control: &SeamControl) -> crate::ConformanceCrash {
    let crash = crate::ConformanceCrash::new();
    let trigger = crash.clone();
    let control = control.clone();
    crate::task::spawn(async move {
        control.wait_for_hit().await;
        control.simulate_process_crash();
        trigger.fire();
    });
    crash
}

/// The admitted scope a runner runs the reference turn under.
fn reference_admitted_scope(identity: &ReferenceIdentity) -> crate::AdmittedScope {
    crate::AdmittedScope::queue_drain(&identity.session_id, identity.turn_id.as_str())
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
                TurnSeamOperation::Effect(
                    EffectOperation::ToolAttempt { .. } | EffectOperation::GroupChild { .. }
                )
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn golden_trace() -> Vec<TurnSeamOperation> {
    serde_json::from_str(GOLDEN_TRACE).expect("committed turn crash trace is valid")
}

/// Re-record the reference turn on the tier's runner and fail if its live
/// seam traffic drifts from the committed golden trace or the outcome table
/// omits a generated point.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn turn_crash_trace_drift_check<F, S>(
    stores: Arc<dyn crate::StoreSet>,
    make: F,
    host: Arc<dyn crate::EffectHost>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) where
    F: Fn(&str) -> Arc<S>,
    S: RuntimePersistence + crate::store::StoreTestSupport + 'static,
{
    let control = SeamControl::default();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let raw = make("trace-drift") as Arc<dyn RuntimePersistence>;
    let identity = ReferenceIdentity::for_scenario("trace-drift");
    seed_reference_ingress(&raw, &identity, "trace-drift").await;
    // The golden trace below is an exact ordering; pin the one seam whose
    // timing is owned by a background timer rather than by the turn.
    let (attempt, reports) = ReferenceTurn::new(
        &stores,
        raw,
        &LawSeamHost::over(host),
        &identity,
        control.clone(),
        &executions,
        nominal_recovery_timings(),
    )
    .before_drive(SeamControl::clear)
    .reporting();
    runner
        .run_turn(reference_admitted_scope(&identity), attempt)
        .await;
    let turn = reference_turn::reported(reports)
        .await
        .map(crate::facade_support::QueuedTurnDrain::ran)
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
    validate_error_return_rulings(&error_return_rulings())
        .unwrap_or_else(|error| panic!("invalid error-return rulings: {error}"));
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
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
fn deferred_to_next_turn(pending_inputs: &[crate::PendingTurnInputRead]) -> bool {
    !pending_inputs.is_empty()
        && pending_inputs
            .iter()
            .all(|read| read.input.state.is_next_turn_pending())
}

/// The reference turn's committed user text for one pending input row.
fn pending_input_text(read: &crate::PendingTurnInputRead) -> String {
    read.input
        .input
        .items
        .iter()
        .map(|item| match item {
            crate::InputItem::Text { text } => text.clone(),
            other => panic!("reference ingress only seeds text inputs, got {other:?}"),
        })
        .collect()
}

/// Run the trace-generated level-1 crash matrix on the tier's runner: every
/// generated crash point kills the reference turn's execution where it
/// stands, and the tier recovers the turn its own way (see the
/// `turn_runner` module docs). `make` returns fresh outer handles over the
/// substrate selected by its semantic scenario key.
///
/// Every generated crash point runs under the tier's session drive.
pub async fn turn_crash_matrix_level_1<F, S>(
    stores: Arc<dyn crate::StoreSet>,
    make: F,
    host: Arc<dyn crate::EffectHost>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) where
    F: Fn(&str) -> Arc<S>,
    S: RuntimePersistence + crate::store::StoreTestSupport + 'static,
{
    Box::pin(turn_crash_matrix_level_1_parking(
        stores,
        make,
        host,
        runner,
        &[],
    ))
    .await;
}

/// A level-one crash point a tier cannot recover yet, parked under the
/// known-defect ticket that brings it back. `point` is the point as the
/// outcome table encodes it (`turn_crash_outcomes.json`).
#[derive(Clone, Copy, Debug)]
pub struct ParkedTurnCrashPoint {
    pub point: &'static str,
    pub ticket: &'static str,
    pub reason: &'static str,
}

/// Every parked point decoded, checked to name a generated crash point once,
/// and to carry a ticket and a reason: a parking that names nothing the
/// matrix runs is stale.
fn validate_parked_points(
    parked: &[ParkedTurnCrashPoint],
) -> Result<Vec<(TurnCrashPoint, ParkedTurnCrashPoint)>, String> {
    let generated = generated_points(&golden_trace());
    let mut decoded: Vec<(TurnCrashPoint, ParkedTurnCrashPoint)> = Vec::new();
    for parking in parked {
        let point: TurnCrashPoint = serde_json::from_str(parking.point)
            .map_err(|error| format!("`{}` is not a crash point: {error}", parking.point))?;
        if !generated.contains(&point) {
            return Err(format!(
                "`{}` is not a generated crash point",
                parking.point
            ));
        }
        if decoded.iter().any(|(seen, _)| *seen == point) {
            return Err(format!("`{}` is parked twice", parking.point));
        }
        if !parking.ticket.starts_with("FIG-") || parking.reason.trim().is_empty() {
            return Err(format!(
                "`{}` must name its FIG ticket and a reason",
                parking.point
            ));
        }
        decoded.push((point, *parking));
    }
    Ok(decoded)
}

/// [`turn_crash_matrix_level_1`] with `parked` crash points held back, each
/// under the known-defect ticket that brings it back. A tier parks a point
/// only where recovery from it hits that defect; every other point runs.
pub async fn turn_crash_matrix_level_1_parking<F, S>(
    stores: Arc<dyn crate::StoreSet>,
    make: F,
    host: Arc<dyn crate::EffectHost>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    parked: &[ParkedTurnCrashPoint],
) where
    F: Fn(&str) -> Arc<S>,
    S: RuntimePersistence + crate::store::StoreTestSupport + 'static,
{
    let make = |scenario: &str| make(scenario) as Arc<dyn RuntimePersistence>;
    let host = LawSeamHost::over(host);
    let law = MatrixLaw {
        stores: &stores,
        make: &make,
        host: &host,
        runner: &runner,
    };
    let parked = validate_parked_points(parked)
        .unwrap_or_else(|error| panic!("invalid parked level-one crash points: {error}"));
    for entry in turn_crash_matrix_outcomes() {
        if let Some(parking) = parked.iter().find(|(point, _)| *point == entry.point) {
            eprintln!(
                "level-one crash point {} parked under {}: {}",
                point_key(&entry.point),
                parking.1.ticket,
                parking.1.reason
            );
            continue;
        }
        let scenario = point_key(&entry.point);
        Box::pin(run_crash_matrix_case(&law, &entry, &scenario)).await;
    }
}

/// What every case of a runner-driven crash-matrix law runs over.
pub(super) struct MatrixLaw<'law> {
    pub(super) stores: &'law Arc<dyn crate::StoreSet>,
    pub(super) make: &'law dyn Fn(&str) -> Arc<dyn RuntimePersistence>,
    /// The law's one layered host over the tier's.
    pub(super) host: &'law LawSeamHost,
    pub(super) runner: &'law Arc<dyn crate::ConformanceTurnRunner>,
}

#[cfg(test)]
mod tests;
