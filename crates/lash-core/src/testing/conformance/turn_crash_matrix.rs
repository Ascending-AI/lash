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
use crate::store::{
    AttachmentIntent, AttachmentManifest, AttachmentManifestEntry, PersistedSessionRead,
    RuntimeCommit, RuntimeCommitResult, SessionCommitStore,
};
use crate::{
    AttachmentId, CheckpointKind, GcReport, LeaseOwnerIdentity, PendingTurnInput,
    PendingTurnInputCancelResult, PendingTurnInputCancelTarget, PendingTurnInputDraft,
    QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaim, QueuedWorkClaimBoundary,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimePersistence, SessionAdmission,
    SessionBinding, SessionExecutionLease, SessionExecutionLeaseAuthority,
    SessionExecutionLeaseClaimOutcome, SessionExecutionLeaseStore, SessionHeadMeta, SessionMeta,
    StoreError, StoreMaintenance, TurnInputApplication, TurnInputClaim, TurnInputStore,
    VacuumReport,
};

const GOLDEN_TRACE: &str = include_str!("turn_crash_trace.json");
const OUTCOME_TABLE: &str = include_str!("turn_crash_outcomes.json");
const RECOVERY_TTL: Duration = Duration::from_millis(300);
const RECOVERY_RENEW: Duration = Duration::from_millis(100);
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
    ClaimSessionExecutionLease,
    RenewSessionExecutionLease,
    ReleaseSessionExecutionLease,
    ClaimLeadingSessionCommand,
    ClaimNextTurnInputs,
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
}

#[derive(Clone, Debug, Default)]
struct SeamControl {
    state: Arc<Mutex<SeamState>>,
    hit: Arc<tokio::sync::Notify>,
    completed: Arc<tokio::sync::Notify>,
}

impl SeamControl {
    fn record(&self, operation: TurnSeamOperation) {
        let mut state = self.state.lock_recover();
        let duplicate_renewal = operation
            == TurnSeamOperation::Store(StoreOperation::RenewSessionExecutionLease)
            && state.trace.contains(&operation);
        if !duplicate_renewal {
            state.trace.push(operation);
        }
    }

    fn arm(&self, point: TurnCrashPoint) {
        let mut state = self.state.lock_recover();
        state.trace.clear();
        state.completed.clear();
        state.armed = Some(point);
        state.hit = false;
    }

    fn clear(&self) {
        let mut state = self.state.lock_recover();
        state.trace.clear();
        state.completed.clear();
        state.armed = None;
        state.hit = false;
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

impl AttachmentManifest for SeamStore {
    fn record_intent(&self, intent: AttachmentIntent) -> Result<(), StoreError> {
        self.inner.record_intent(intent)
    }

    fn commit_refs(&self, session_id: &str, ids: &[AttachmentId]) -> Result<(), StoreError> {
        self.inner.commit_refs(session_id, ids)
    }

    fn list_uncommitted(
        &self,
        older_than: u64,
    ) -> Result<Vec<AttachmentManifestEntry>, StoreError> {
        self.inner.list_uncommitted(older_than)
    }

    fn forget_aged_uncommitted_intents(&self, cutoff: u64) -> Result<(), StoreError> {
        self.inner.forget_aged_uncommitted_intents(cutoff)
    }

    fn has_live_ref_for_id(&self, id: &AttachmentId, cutoff: u64) -> Result<bool, StoreError> {
        self.inner.has_live_ref_for_id(id, cutoff)
    }

    fn forget(&self, session_id: &str, id: &AttachmentId) -> Result<(), StoreError> {
        self.inner.forget(session_id, id)
    }

    fn holds_ref(&self, session_id: &str, id: &AttachmentId) -> Result<bool, StoreError> {
        self.inner.holds_ref(session_id, id)
    }

    fn list_all_refs(&self) -> Result<Vec<AttachmentId>, StoreError> {
        self.inner.list_all_refs()
    }
}

#[async_trait::async_trait]
impl SessionCommitStore for SeamStore {
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
                TurnSeamOperation::Store(StoreOperation::LoadSession),
                self.inner.load_session_head_meta(),
            )
            .await
    }

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<crate::SessionNodeRecord>, StoreError> {
        self.inner.load_node(node_id).await
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitResult, StoreError> {
        let operation = TurnSeamOperation::Store(StoreOperation::CommitFinalHead {
            settles_queue: !commit.completed_queue_claims.is_empty(),
            settles_turn_input: !commit.completed_turn_input_claims.is_empty(),
            releases_lease: commit.release_session_execution_lease.is_some(),
        });
        self.control
            .around(operation, self.inner.commit_runtime_state(commit))
            .await
    }

    async fn admit_and_bind_session(
        &self,
        binding: &SessionBinding,
    ) -> Result<SessionAdmission, StoreError> {
        self.inner.admit_and_bind_session(binding).await
    }

    async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError> {
        self.inner.save_session_meta(meta).await
    }

    async fn load_session_meta(&self) -> Result<Option<SessionMeta>, StoreError> {
        self.inner.load_session_meta().await
    }
}

#[async_trait::async_trait]
impl TurnInputStore for SeamStore {
    async fn enqueue_pending_turn_input(
        &self,
        input: PendingTurnInputDraft,
    ) -> Result<PendingTurnInput, StoreError> {
        self.inner.enqueue_pending_turn_input(input).await
    }

    async fn list_pending_turn_inputs(
        &self,
        session_id: &str,
    ) -> Result<Vec<PendingTurnInput>, StoreError> {
        self.inner.list_pending_turn_inputs(session_id).await
    }

    async fn list_turn_input_applications(
        &self,
        session_id: &str,
    ) -> Result<Vec<TurnInputApplication>, StoreError> {
        self.inner.list_turn_input_applications(session_id).await
    }

    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &str,
        targets: &[PendingTurnInputCancelTarget],
    ) -> Result<Vec<PendingTurnInputCancelResult>, StoreError> {
        self.inner
            .cancel_pending_turn_inputs(session_id, targets)
            .await
    }

    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &str,
        anchor: &PendingTurnInputCancelTarget,
    ) -> Result<crate::PendingTurnInputSuffixCancelOutcome, StoreError> {
        self.inner
            .cancel_pending_turn_input_suffix(session_id, anchor)
            .await
    }

    async fn claim_active_turn_inputs(
        &self,
        session_id: &str,
        fence: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &crate::TurnId,
        checkpoint: CheckpointKind,
        max_inputs: usize,
    ) -> Result<Option<TurnInputClaim>, StoreError> {
        self.inner
            .claim_active_turn_inputs(session_id, fence, owner, turn_id, checkpoint, max_inputs)
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

    async fn abandon_turn_input_claim(&self, claim: &TurnInputClaim) -> Result<(), StoreError> {
        self.inner.abandon_turn_input_claim(claim).await
    }

    async fn abandon_turn_input_claims(&self, claims: &[TurnInputClaim]) -> Result<(), StoreError> {
        self.inner.abandon_turn_input_claims(claims).await
    }
}

#[async_trait::async_trait]
impl SessionExecutionLeaseStore for SeamStore {
    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
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
        let operation = TurnSeamOperation::Store(StoreOperation::ReleaseSessionExecutionLease);
        self.control
            .around(
                operation,
                self.inner.release_session_execution_lease(completion),
            )
            .await
    }

    /// Passed straight through, deliberately without a seam operation: the
    /// diagnostic read is non-mutating and must never be a crash point, or the
    /// matrix would be injecting faults into observation rather than into the
    /// turn.
    async fn get_session_execution_lease(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionExecutionLease>, StoreError> {
        self.inner.get_session_execution_lease(session_id).await
    }
}

#[async_trait::async_trait]
impl crate::QueuedWorkStore for SeamStore {
    async fn enqueue_queued_work(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<QueuedWorkBatch, StoreError> {
        self.inner.enqueue_queued_work(batch).await
    }

    async fn enqueue_queued_work_with_outcome(
        &self,
        batch: QueuedWorkBatchDraft,
    ) -> Result<crate::QueuedWorkEnqueueOutcome, StoreError> {
        self.inner.enqueue_queued_work_with_outcome(batch).await
    }

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
        max_batches: usize,
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        let operation = TurnSeamOperation::Store(StoreOperation::ClaimReadyQueuedWork {
            boundary: format!("{boundary:?}").to_ascii_lowercase(),
        });
        self.control
            .around(
                operation,
                self.inner
                    .claim_ready_queued_work(session_id, fence, owner, boundary, max_batches),
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
        max_batches: usize,
    ) -> Result<(Option<TurnInputClaim>, Option<QueuedWorkClaim>), StoreError> {
        let operation = TurnSeamOperation::Store(StoreOperation::ClaimCheckpointWork {
            checkpoint: format!("{checkpoint:?}").to_ascii_lowercase(),
        });
        self.control
            .around(
                operation,
                self.inner.claim_checkpoint_work(
                    session_id,
                    fence,
                    owner,
                    turn_id,
                    checkpoint,
                    max_inputs,
                    max_batches,
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
    ) -> Result<Option<QueuedWorkClaim>, StoreError> {
        let operation = TurnSeamOperation::Store(StoreOperation::ClaimSelectedQueuedWork {
            boundary: format!("{boundary:?}").to_ascii_lowercase(),
        });
        self.control
            .around(
                operation,
                self.inner
                    .claim_ready_queued_work_by_batch_ids(session_id, fence, owner, boundary, ids),
            )
            .await
    }

    async fn abandon_queued_work_claim(&self, claim: &QueuedWorkClaim) -> Result<(), StoreError> {
        self.inner.abandon_queued_work_claim(claim).await
    }

    async fn abandon_queued_work_claims(
        &self,
        claims: &[QueuedWorkClaim],
    ) -> Result<(), StoreError> {
        self.inner.abandon_queued_work_claims(claims).await
    }

    async fn cancel_queued_work_batch(
        &self,
        session_id: &str,
        batch_id: &str,
    ) -> Result<Option<QueuedWorkBatch>, StoreError> {
        self.inner
            .cancel_queued_work_batch(session_id, batch_id)
            .await
    }

    async fn list_queued_work(&self, session_id: &str) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        self.inner.list_queued_work(session_id).await
    }

    async fn list_pending_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<QueuedWorkBatch>, StoreError> {
        self.inner.list_pending_queued_work(session_id).await
    }
}

#[async_trait::async_trait]
impl StoreMaintenance for SeamStore {
    async fn vacuum(&self) -> Result<VacuumReport, StoreError> {
        self.inner.vacuum().await
    }
    async fn gc_unreachable(&self) -> Result<GcReport, StoreError> {
        self.inner.gc_unreachable().await
    }
    async fn seed_session_trigger_manifest_ref_for_testing(
        &self,
        session_id: &str,
    ) -> Result<bool, StoreError> {
        self.inner
            .seed_session_trigger_manifest_ref_for_testing(session_id)
            .await
    }
    async fn raw_session_owned_artifact_refs_for_testing(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        self.inner
            .raw_session_owned_artifact_refs_for_testing(session_id)
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
    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolResult {
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
        crate::ToolResult::ok(serde_json::json!({"effect":"executed"}))
    }
}

fn recovery_timings() -> crate::LeaseTimings {
    crate::LeaseTimings::new(RECOVERY_TTL, RECOVERY_RENEW)
        .expect("300ms TTL / 100ms renew satisfies ttl >= 3x renew")
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
    mut trace_tool: TraceTool,
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
    )
    .with_lease_timings(recovery_timings());
    trace_tool.control = control.clone();
    host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(provider_handle(control)));
    let mut plugin_factories = crate::testing::test_standard_protocol_factories();
    plugin_factories.push(Arc::new(StaticPluginFactory::new(
        "turn_crash_trace_tool",
        PluginSpec::new().with_tool_provider(Arc::new(trace_tool)),
    )));
    Box::pin(
        crate::LashRuntime::builder(crate::CommitBudget::bounded(1024 * 1024, 512))
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

async fn seed_reference_ingress(store: &Arc<dyn RuntimePersistence>, identity: &ReferenceIdentity) {
    super::bind_conformance_session(store, &identity.session_id).await;
    store
        .enqueue_pending_turn_input(PendingTurnInputDraft::new(
            &identity.session_id,
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("durable next-turn input"),
        ))
        .await
        .expect("seed next-turn input");
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
                crate::SlotPolicy::Exclusive,
                vec![crate::QueuedWorkPayload::agent_frame_task(
                    "trace-frame",
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

/// Level-2 crash sites driven by the backend helper processes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ColdProcessTurnAction {
    ProviderInitialMidStream,
    ProviderAfterToolMidStream,
    EffectAfterExternalBeforeOutcome,
    FinalCommitBoundary,
    FinalCommitInsideCall,
    CheckpointAfterExecuteBeforeOutcome,
    RecoverFinalCommitBoundary,
    PeerReclaim,
    Recover,
}

impl ColdProcessTurnAction {
    const CRASH_ACTIONS: [Self; 5] = [
        Self::ProviderInitialMidStream,
        Self::ProviderAfterToolMidStream,
        Self::EffectAfterExternalBeforeOutcome,
        Self::FinalCommitBoundary,
        Self::FinalCommitInsideCall,
    ];

    fn command(self) -> &'static str {
        match self {
            Self::ProviderInitialMidStream => "turn_provider_mid_stream",
            Self::ProviderAfterToolMidStream => "turn_provider_after_tool_mid_stream",
            Self::EffectAfterExternalBeforeOutcome => "turn_effect_after_external",
            Self::FinalCommitBoundary => "turn_final_commit_boundary",
            Self::FinalCommitInsideCall => "turn_final_commit_inside",
            Self::CheckpointAfterExecuteBeforeOutcome => {
                "turn_checkpoint_after_execute_before_outcome"
            }
            Self::RecoverFinalCommitBoundary => "turn_recover_final_commit_boundary",
            Self::PeerReclaim => "turn_peer_reclaim",
            Self::Recover => "turn_recover",
        }
    }

    fn point(self) -> Option<TurnCrashPoint> {
        match self {
            Self::ProviderInitialMidStream => Some(TurnCrashPoint {
                operation: TurnSeamOperation::Provider(ProviderOperation::InitialMidStream),
                placement: CrashPlacement::ProviderMidStream,
            }),
            Self::ProviderAfterToolMidStream => Some(TurnCrashPoint {
                operation: TurnSeamOperation::Provider(ProviderOperation::AfterToolMidStream),
                placement: CrashPlacement::ProviderMidStream,
            }),
            Self::EffectAfterExternalBeforeOutcome => Some(TurnCrashPoint {
                operation: TurnSeamOperation::Effect(EffectOperation::ToolAttempt {
                    name: "trace_effect".to_string(),
                }),
                placement: CrashPlacement::AfterExternalEffectBeforeOutcome,
            }),
            Self::FinalCommitBoundary | Self::RecoverFinalCommitBoundary => Some(TurnCrashPoint {
                operation: TurnSeamOperation::Store(StoreOperation::CommitFinalHead {
                    settles_queue: true,
                    settles_turn_input: true,
                    releases_lease: true,
                }),
                placement: CrashPlacement::Boundary,
            }),
            Self::FinalCommitInsideCall => Some(TurnCrashPoint {
                operation: TurnSeamOperation::Store(StoreOperation::CommitFinalHead {
                    settles_queue: true,
                    settles_turn_input: true,
                    releases_lease: true,
                }),
                placement: CrashPlacement::InsideCall,
            }),
            Self::CheckpointAfterExecuteBeforeOutcome | Self::PeerReclaim | Self::Recover => None,
        }
    }
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
    const EXPECTED_SCENARIOS: [&str; 3] = [
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

/// Return each helper action's exact effect-count and durable-state oracle.
///
/// The values are derived from `turn_crash_outcomes.json`, keeping the backend
/// helper-process assertions on the same oracle as the level-1 matrix.
pub fn cold_process_turn_expectations() -> Vec<(&'static str, usize, usize, String, Option<String>)>
{
    let generated = generated_points(&golden_trace());
    let table = turn_crash_matrix_outcomes();
    validate_outcome_table(&generated, &table).expect("committed turn crash outcomes are valid");
    validate_durable_recovery_rulings(&durable_recovery_rulings())
        .expect("committed durable recovery rulings are valid");
    ColdProcessTurnAction::CRASH_ACTIONS
        .into_iter()
        .map(|action| {
            let point = action.point().expect("crash action has a point");
            let expectation = table
                .iter()
                .find(|entry| entry.point == point)
                .and_then(|entry| entry.level_2.as_ref())
                .expect("level-2 action has a committed expectation");
            let (end_state, known_defect) = match (&expectation.exact, &expectation.known_defect) {
                (Some(exact), None) => (
                    exact.exact().expect("validated exact end-state expectation"),
                    None,
                ),
                (None, Some(defect)) => {
                    let expected = defect
                        .expected_defective
                        .exact()
                        .expect("validated exact known-defect expectation");
                    let notice = format!(
                        "KNOWN-DEFECT {} reproduced exactly for {}: observed {}; fixing {} must produce the correct durable end state {}",
                        defect.ticket,
                        action.command(),
                        expected.summary(),
                        defect.ticket,
                        DurableEndState::CORRECT.summary()
                    );
                    (expected, Some(notice))
                }
                _ => unreachable!("validated level-2 end-state expectation"),
            };
            (
                action.command(),
                expectation.effect_executions.at_crash,
                expectation.effect_executions.after_recovery,
                end_state.summary(),
                known_defect,
            )
        })
        .collect()
}

/// Return the reviewed exact durable end state for a composed level-2 recovery
/// trajectory in `turn_crash_outcomes.json`.
pub fn cold_process_durable_recovery_expectation(scenario: &str) -> String {
    let rulings = durable_recovery_rulings();
    validate_durable_recovery_rulings(&rulings)
        .expect("committed durable recovery rulings are valid");
    rulings
        .into_iter()
        .find(|ruling| ruling.scenario == scenario)
        .unwrap_or_else(|| panic!("unknown durable recovery scenario `{scenario}`"))
        .exact
        .exact()
        .expect("validated exact durable recovery ruling")
        .summary()
}

/// Return the stable execution scope used by a level-2 helper scenario.
pub fn cold_process_turn_scope(scenario: &str) -> crate::ExecutionScope {
    let identity = ReferenceIdentity::for_scenario(scenario);
    crate::ExecutionScope::turn(identity.session_id, identity.turn_id)
}

/// Drive or recover one full scripted turn inside a backend helper process.
///
/// `action` accepts `turn_provider_mid_stream`,
/// `turn_provider_after_tool_mid_stream`, `turn_effect_after_external`,
/// `turn_final_commit_boundary`, `turn_final_commit_inside`, or
/// `turn_recover`.
///
/// Crash actions print `crash_ready` only after the configured semantic point
/// is reached and then park until the parent sends `SIGKILL`. `Recover` polls
/// the session lease, drives a fresh runtime/controller, and reports exact
/// committed terminal-output and ingress counts from the reopened store. The
/// parent process compares that `turn_complete` summary with the outcome table.
pub async fn cold_process_real_turn_driver(
    store: Arc<dyn RuntimePersistence>,
    effect_controller: Arc<dyn RuntimeEffectController>,
    scenario: &str,
    action: &str,
    external_effect_marker: Option<std::path::PathBuf>,
) {
    let action = match action {
        "turn_provider_mid_stream" => ColdProcessTurnAction::ProviderInitialMidStream,
        "turn_provider_after_tool_mid_stream" => ColdProcessTurnAction::ProviderAfterToolMidStream,
        "turn_effect_after_external" => ColdProcessTurnAction::EffectAfterExternalBeforeOutcome,
        "turn_final_commit_boundary" => ColdProcessTurnAction::FinalCommitBoundary,
        "turn_final_commit_inside" => ColdProcessTurnAction::FinalCommitInsideCall,
        "turn_checkpoint_after_execute_before_outcome" => {
            ColdProcessTurnAction::CheckpointAfterExecuteBeforeOutcome
        }
        "turn_recover_final_commit_boundary" => ColdProcessTurnAction::RecoverFinalCommitBoundary,
        "turn_peer_reclaim" => ColdProcessTurnAction::PeerReclaim,
        "turn_recover" => ColdProcessTurnAction::Recover,
        other => panic!("unknown cold-process real-turn action `{other}`"),
    };
    let identity = ReferenceIdentity::for_scenario(scenario);
    let control = SeamControl::default();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let recovers_existing_turn = matches!(
        action,
        ColdProcessTurnAction::Recover
            | ColdProcessTurnAction::RecoverFinalCommitBoundary
            | ColdProcessTurnAction::PeerReclaim
    );
    if !recovers_existing_turn {
        seed_reference_ingress(&store, &identity).await;
    } else if action == ColdProcessTurnAction::PeerReclaim {
        let owner =
            LeaseOwnerIdentity::opaque("cold-process-peer", format!("{scenario}:peer-reclaim"));
        let lease = tokio::time::timeout(RECOVERY_TIMEOUT, async {
            loop {
                super::bind_conformance_session(&store, &identity.session_id).await;
                let outcome = store
                    .try_claim_session_execution_lease(
                        &identity.session_id,
                        &owner,
                        recovery_timings().ttl_ms(),
                    )
                    .await
                    .expect("poll peer-reclaim lease");
                if let Some(lease) = outcome.acquired() {
                    break lease;
                }
                tokio::time::sleep(recovery_timings().renew_interval()).await;
            }
        })
        .await
        .expect("peer can acquire crashed turn lease");
        let claim = store
            .claim_ready_queued_work(
                &identity.session_id,
                &lease.fence(),
                &owner,
                crate::QueuedWorkClaimBoundary::Idle,
                64,
            )
            .await
            .expect("peer reclaims queued-work row")
            .expect("crashed turn left one queued-work row");
        assert_eq!(claim.batches.len(), 1, "peer reclaims exactly one row");
        store
            .release_session_execution_lease(&lease.completion())
            .await
            .expect("release peer lease without settling peer row");
        println!(
            "peer_claim row={} claim={} generation={}",
            claim.batches[0].batch_id, claim.claim_id, claim.session_lease_generation
        );
        return;
    } else {
        let owner = LeaseOwnerIdentity::opaque(
            "cold-process-recovery-probe",
            format!("{scenario}:recovery-probe"),
        );
        tokio::time::timeout(RECOVERY_TIMEOUT, async {
            loop {
                super::bind_conformance_session(&store, &identity.session_id).await;
                let outcome = store
                    .try_claim_session_execution_lease(
                        &identity.session_id,
                        &owner,
                        recovery_timings().ttl_ms(),
                    )
                    .await
                    .expect("poll cold-process recovery lease");
                if let Some(lease) = outcome.acquired() {
                    store
                        .release_session_execution_lease(&lease.completion())
                        .await
                        .expect("release cold-process recovery probe");
                    break;
                }
                tokio::time::sleep(recovery_timings().renew_interval()).await;
            }
        })
        .await
        .expect("cold-process turn lease becomes reclaimable");
    }

    let trace_tool = TraceTool {
        marker: external_effect_marker,
        ..TraceTool::default()
    };
    let reader = Arc::clone(&store);
    let decorated = SeamStore::wrap(store, control.clone());
    let runtime = Box::pin(build_runtime(
        decorated,
        control.clone(),
        executions,
        &identity,
        trace_tool,
    ))
    .await;

    let point = action.point();
    if let Some(point) = point {
        control.arm(point);
    } else {
        control.clear();
    }
    let effect_controller: Arc<dyn RuntimeEffectController> =
        if action == ColdProcessTurnAction::CheckpointAfterExecuteBeforeOutcome {
            Arc::new(CrashAfterCheckpointExecutionController {
                inner: effect_controller,
            })
        } else {
            effect_controller
        };
    let task_identity = identity.clone();
    let task =
        crate::task::spawn(
            async move { drive_turn(runtime, effect_controller, &task_identity).await },
        );
    if action == ColdProcessTurnAction::CheckpointAfterExecuteBeforeOutcome {
        let result = task.await;
        panic!(
            "checkpoint crash controller returned instead of terminating the process: {result:?}"
        );
    }
    if action != ColdProcessTurnAction::Recover {
        control.wait_for_hit().await;
        println!("crash_ready");
        std::io::Write::flush(&mut std::io::stdout()).expect("flush level-2 crash signal");
        std::future::pending::<()>().await;
    }
    let _recovered_turn = task
        .await
        .expect("join cold-process recovered turn")
        .expect("drive cold-process recovered turn");
    let state = crate::load_persisted_session_state(reader.as_ref())
        .await
        .expect("read cold-process recovered state")
        .expect("cold-process recovery committed a session head");
    let terminal_count = state
        .session_graph
        .read_model()
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| part.content == "trace turn complete")
        .count();
    let pending_input_count = reader
        .list_pending_turn_inputs(&identity.session_id)
        .await
        .expect("list cold-process recovered turn inputs")
        .len();
    let queued_work_count = reader
        .list_queued_work(&identity.session_id)
        .await
        .expect("list cold-process recovered queued work")
        .len();
    println!(
        "turn_complete terminal={terminal_count} pending_inputs={pending_input_count} queued_work={queued_work_count}"
    );
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
    seed_reference_ingress(&raw, &identity).await;
    let decorated = SeamStore::wrap(raw, control.clone());
    let runtime = Box::pin(build_runtime(
        decorated,
        control.clone(),
        Arc::clone(&executions),
        &identity,
        TraceTool::default(),
    ))
    .await;
    control.clear();
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

async fn wait_for_recovery_lease<F>(make: &F, scenario: &str)
where
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
                    recovery_timings().ttl_ms(),
                )
                .await
                .expect("probe recovery lease");
            if let Some(lease) = outcome.acquired() {
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

/// Run the trace-generated level-1 crash matrix against one persistence
/// backend. The factory must return fresh outer handles over the substrate
/// selected by its semantic scenario key.
pub async fn turn_crash_matrix_level_1<F>(make: F)
where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
{
    Box::pin(turn_crash_trace_drift_check(&make)).await;
    for entry in turn_crash_matrix_outcomes() {
        let scenario = point_key(&entry.point);
        let identity = ReferenceIdentity::for_scenario(&scenario);
        let raw = make(&scenario);
        seed_reference_ingress(&raw, &identity).await;
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
        let task = crate::task::spawn(async move {
            drive_turn(runtime, effect_controller, &task_identity).await
        });
        control.wait_for_hit().await;
        task.abort();
        let _ = task.await;

        wait_for_recovery_lease(&make, &scenario).await;
        let successor_control = SeamControl::default();
        let successor_store = SeamStore::wrap(make(&scenario), successor_control.clone());
        let successor = Box::pin(build_runtime(
            Arc::clone(&successor_store),
            successor_control.clone(),
            Arc::clone(&executions),
            &identity,
            TraceTool::default(),
        ))
        .await;
        successor_control.clear();
        let successor_effect_controller: Arc<dyn RuntimeEffectController> =
            Arc::new(SeamEffectController {
                inner: Arc::new(crate::InlineRuntimeEffectController::default()),
                control: successor_control,
                executions: Arc::clone(&executions),
            });
        let _ = drive_turn(successor, successor_effect_controller, &identity)
            .await
            .unwrap_or_else(|error| panic!("successor failed for {scenario} ({entry:?}): {error}"));

        let reader = make(&scenario);
        super::bind_conformance_session(&reader, &identity.session_id).await;
        let state = crate::load_persisted_session_state(reader.as_ref())
            .await
            .expect("read recovered state")
            .expect("recovered turn commits state");
        let terminal_count = state
            .session_graph
            .read_model()
            .messages
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter(|part| part.content == "trace turn complete")
            .count();
        assert_eq!(
            terminal_count, 1,
            "{scenario} ({entry:?}): recovery must expose one terminal assistant output"
        );
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
            effect_count, entry.effect_executions_l1,
            "{scenario}: {}",
            entry.outcome
        );
    }
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
                        operation: TurnSeamOperation::Store(StoreOperation::LoadSession),
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
