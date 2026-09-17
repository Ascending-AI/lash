//! Model-based [`RuntimePersistence`] laws for leases, queues, inputs, commit
//! CAS, and checkpoint components; process-scoped laws live in the sibling harness.

use super::*;
use crate::StoreError::SessionExecutionLeaseRenewalRefused as RenewalRefused;
use crate::store::{
    EXECUTION_STATE_CHECKPOINT_COMPONENT, PLUGIN_STATE_CHECKPOINT_COMPONENT,
    TOOL_STATE_CHECKPOINT_COMPONENT,
};
use crate::{
    LeaseOwnerIdentity, PendingTurnInput, PendingTurnInputCancelOutcome, PendingTurnInputDraft,
    PluginNamespaceState, PluginState, QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaim,
    QueuedWorkClaimBoundary, RuntimeCommit, RuntimePersistence, RuntimeSessionState,
    RuntimeUsageDeltaIdentity, SessionExecutionLease, SessionExecutionLeaseClaimOutcome,
    StoreError, ToolState, TurnInput, TurnInputClaim, TurnInputIngress,
    facade_support::ToolStateFacadeOps,
};
use lash_core::testing::conformance_support::ToolStateConformanceAccess;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
mod attachment_conservation;
mod claim_honesty;
mod counterexample;
mod dedicated_laws;
mod generator;
mod interrupted_claim_laws;
mod pending_input_read_model;
#[cfg(test)]
mod tests;
mod usage_conservation;
pub use attachment_conservation::RuntimePersistenceStateMachineHandles;
use attachment_conservation::{apply_attachment_operation, assert_attachment_conservation};
use counterexample::persist_counterexample;
use dedicated_laws::assert_dedicated_laws;
use generator::{component_selection, generated_case, plugin_state};
use usage_conservation::{
    assert_usage_conservation, confirm_usage, record_usage, register_committed_usage,
    replay_usage_receipt, stage_usage,
};
const SESSION_ID: &str = "runtime-persistence-property";

/// The property session's identity where a typed one is wanted; the `&str`
/// constant above stays the single source of the bytes.
fn session_id() -> SessionId {
    SessionId::from(SESSION_ID)
}

const DEFAULT_CASES: u32 = 32;
const DEFAULT_RUNNER_SEED: u64 = 857;
const DEDICATED_LAW_SEED: u64 = 0x0ded_1ca7_e857;
const MAX_OPS: usize = 96;
/// The generated operation alphabet shared by every runtime-persistence backend.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RuntimePersistenceOp {
    ClaimLease {
        owner: u8,
    },
    RenewLease {
        stale: bool,
    },
    Crash,
    EnqueueWork {
        slot: u8,
        value: u8,
        coalesce: bool,
    },
    ClaimWork {
        selected: bool,
        selection: u8,
    },
    ClaimWorkWithStaleLease,
    CancelWork {
        selection: u8,
    },
    EnqueueTurnInput {
        slot: u8,
        value: u8,
    },
    ClaimTurnInputs {
        max_inputs: u8,
    },
    ClaimTurnInputsWithStaleLease,
    CancelTurnInput {
        selection: u8,
    },
    RecordUsage {
        slot: u8,
        value: u8,
    },
    StageUsage {
        replay_last_commit: bool,
    },
    ConfirmUsage {
        selection: u8,
    },
    ReplayUsageReceipt,
    CommitWithAttachmentRefs {
        new_session: bool,
        session_selection: u8,
        attachment_slot: u8,
        value: u8,
        #[serde(default)]
        turn_owned: bool,
    },
    PutAttachmentIntent {
        owner_kind: u8,
        attachment_slot: u8,
        value: u8,
    },
    ReplayAttachmentCommit {
        selection: u8,
    },
    ReclaimAttachmentSession {
        selection: u8,
    },
    ProbeAttachmentGc,
    Commit {
        component_mode: u8,
        value: u8,
        settle_work: bool,
        settle_inputs: bool,
        stale_head: bool,
    },
    SettleStaleWork,
    SettleStaleTurnInputs,
}
#[derive(Clone, Debug, serde::Deserialize)]
struct GeneratedCase {
    seed: u64,
    operations: Vec<RuntimePersistenceOp>,
}
#[derive(Clone)]
struct ModeledWork {
    batch: QueuedWorkBatch,
}

#[derive(Clone)]
struct ModeledInput {
    input: PendingTurnInput,
}

#[derive(Clone, Default)]
struct ComponentModel {
    tool_value: Option<u8>,
    tool_ref: Option<crate::BlobRef>,
    plugin_value: Option<u8>,
    plugin_ref: Option<crate::BlobRef>,
    execution_value: Option<u8>,
    execution_ref: Option<crate::BlobRef>,
}

#[derive(Default)]
struct ReferenceModel {
    head_revision: u64,
    has_session: bool,
    current_lease: Option<SessionExecutionLease>,
    stale_leases: Vec<SessionExecutionLease>,
    work: BTreeMap<String, ModeledWork>,
    inputs: BTreeMap<String, ModeledInput>,
    input_receipts: BTreeMap<String, PendingTurnInputDraft>,
    active_work_claims: Vec<QueuedWorkClaim>,
    stale_work_claims: Vec<QueuedWorkClaim>,
    active_input_claims: Vec<TurnInputClaim>,
    stale_input_claims: Vec<TurnInputClaim>,
    applications: Vec<crate::TurnInputApplication>,
    components: ComponentModel,
    crashed_work: BTreeSet<lash_core::BatchId>,
    crashed_inputs: BTreeSet<lash_core::InputId>,
    pending_usage: Arc<
        std::sync::Mutex<Vec<lash_core::testing::conformance_support::PendingTokenLedgerEntry>>,
    >,
    staged_usage: Option<lash_core::testing::conformance_support::StagedTokenLedger>,
    staged_usage_operation: Option<crate::OperationId>,
    pending_usage_confirmations: Vec<PendingUsageConfirmation>,
    durable_usage: HashMap<RuntimeUsageDeltaIdentity, crate::TokenLedgerEntry>,
    recorded_usage: crate::TokenUsage,
    last_usage_commit: Option<RuntimeCommit>,
    attachment_sessions: Vec<attachment_conservation::ModeledAttachmentSession>,
    attachment_ids_to_reprobe: BTreeSet<crate::AttachmentId>,
    live_uncommitted_attachment_refs: BTreeSet<crate::AttachmentId>,
    attachment_session_sequence: u64,
    operation_sequence: u64,
}

struct PendingUsageConfirmation {
    staged: lash_core::testing::conformance_support::StagedTokenLedger,
    identities: Vec<RuntimeUsageDeltaIdentity>,
}
/// The run-shape counter alphabet. `RunShape`, `RunShapeTotals`, the
/// required-shape table, and the report all derive from this one enum, so a
/// new counter cannot be counted without being gated and reported.
#[derive(Clone, Copy, Debug)]
enum RunShapeCounter {
    LeaseAcquisitions,
    LeaseFenceRejections,
    QueueEnqueues,
    QueueClaims,
    SelectedBatchClaims,
    QueueCompletions,
    ClaimSupersessionRejections,
    StaleClaimSettlements,
    OutOfOrderSettlements,
    CoalescedClaims,
    QueueCancellations,
    InputEnqueues,
    InputClaims,
    InputApplications,
    InputCancellations,
    UsageRecords,
    UsageStages,
    UsageConfirmations,
    UsageReceiptReplays,
    AttachmentCommits,
    AttachmentIntentPuts,
    AttachmentReceiptReplays,
    AttachmentSessionReclaims,
    AttachmentGcProbes,
    AcceptedCommits,
    StaleHeadRejections,
    CheckpointStores,
    CheckpointRefReuses,
    CheckpointClears,
    CrashPoints,
    CrashReclaims,
}

impl RunShapeCounter {
    const ALL: &[Self] = &[
        Self::LeaseAcquisitions,
        Self::LeaseFenceRejections,
        Self::QueueEnqueues,
        Self::QueueClaims,
        Self::SelectedBatchClaims,
        Self::QueueCompletions,
        Self::ClaimSupersessionRejections,
        Self::StaleClaimSettlements,
        Self::OutOfOrderSettlements,
        Self::CoalescedClaims,
        Self::QueueCancellations,
        Self::InputEnqueues,
        Self::InputClaims,
        Self::InputApplications,
        Self::InputCancellations,
        Self::UsageRecords,
        Self::UsageStages,
        Self::UsageConfirmations,
        Self::UsageReceiptReplays,
        Self::AttachmentCommits,
        Self::AttachmentIntentPuts,
        Self::AttachmentReceiptReplays,
        Self::AttachmentSessionReclaims,
        Self::AttachmentGcProbes,
        Self::AcceptedCommits,
        Self::StaleHeadRejections,
        Self::CheckpointStores,
        Self::CheckpointRefReuses,
        Self::CheckpointClears,
        Self::CrashPoints,
        Self::CrashReclaims,
    ];
    const COUNT: usize = Self::ALL.len();

    fn name(self) -> &'static str {
        match self {
            Self::LeaseAcquisitions => "lease_acquisitions",
            Self::LeaseFenceRejections => "lease_fence_rejections",
            Self::QueueEnqueues => "queue_enqueues",
            Self::QueueClaims => "queue_claims",
            Self::SelectedBatchClaims => "selected_batch_claims",
            Self::QueueCompletions => "queue_completions",
            Self::ClaimSupersessionRejections => "claim_supersession_rejections",
            Self::StaleClaimSettlements => "stale_claim_settlements",
            Self::OutOfOrderSettlements => "out_of_order_settlements",
            Self::CoalescedClaims => "coalesced_claims",
            Self::QueueCancellations => "queue_cancellations",
            Self::InputEnqueues => "input_enqueues",
            Self::InputClaims => "input_claims",
            Self::InputApplications => "input_applications",
            Self::InputCancellations => "input_cancellations",
            Self::UsageRecords => "usage_records",
            Self::UsageStages => "usage_stages",
            Self::UsageConfirmations => "usage_confirmations",
            Self::UsageReceiptReplays => "usage_receipt_replays",
            Self::AttachmentCommits => "attachment_commits",
            Self::AttachmentIntentPuts => "attachment_intent_puts",
            Self::AttachmentReceiptReplays => "attachment_receipt_replays",
            Self::AttachmentSessionReclaims => "attachment_session_reclaims",
            Self::AttachmentGcProbes => "attachment_gc_probes",
            Self::AcceptedCommits => "accepted_commits",
            Self::StaleHeadRejections => "stale_head_rejections",
            Self::CheckpointStores => "checkpoint_stores",
            Self::CheckpointRefReuses => "checkpoint_ref_reuses",
            Self::CheckpointClears => "checkpoint_clears",
            Self::CrashPoints => "crash_points",
            Self::CrashReclaims => "crash_reclaims",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct RunShape {
    counts: [u64; RunShapeCounter::COUNT],
}

impl std::ops::Index<RunShapeCounter> for RunShape {
    type Output = u64;
    fn index(&self, counter: RunShapeCounter) -> &u64 {
        &self.counts[counter as usize]
    }
}

impl std::ops::IndexMut<RunShapeCounter> for RunShape {
    fn index_mut(&mut self, counter: RunShapeCounter) -> &mut u64 {
        &mut self.counts[counter as usize]
    }
}

#[derive(Debug)]
struct RunShapeTotals {
    counts: [AtomicU64; RunShapeCounter::COUNT],
}

impl Default for RunShapeTotals {
    fn default() -> Self {
        Self {
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl RunShapeTotals {
    fn add(&self, shape: RunShape) {
        for counter in RunShapeCounter::ALL {
            self.counts[*counter as usize].fetch_add(shape[*counter], Ordering::Relaxed);
        }
    }

    fn report(&self) -> String {
        RunShapeCounter::ALL
            .iter()
            .map(|counter| {
                format!(
                    "{}={}",
                    counter.name(),
                    self.counts[*counter as usize].load(Ordering::Relaxed)
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Run generated runtime-persistence laws with shrinking and persisted counterexamples.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn runtime_persistence_state_machine<F, Fut>(backend: &'static str, make: F)
where
    F: Fn(u64) -> Fut + Send + Sync + Clone + 'static,
    Fut: Future<Output = RuntimePersistenceStateMachineHandles> + Send + 'static,
{
    let first = make(u64::MAX - 3).await;
    let second = make(u64::MAX - 3).await;
    assert!(
        !Arc::ptr_eq(&first.runtime, &second.runtime),
        "runtime_persistence_state_machine factory reused one Arc"
    );
    drop((first, second));
    let cases = std::env::var("LASH_RUNTIME_PERSISTENCE_PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_CASES);
    let runner_seed = std::env::var("LASH_RUNTIME_PERSISTENCE_PROPTEST_SEED")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_RUNNER_SEED);

    assert_dedicated_laws(&make, DEDICATED_LAW_SEED)
        .await
        .unwrap_or_else(|error| {
            panic!("{backend} dedicated runtime-persistence law failed: {error}")
        });
    claim_honesty::non_law_pre_reclaim_commit_symmetry(&make, DEDICATED_LAW_SEED + 10)
        .await
        .unwrap_or_else(|error| {
            panic!("{backend} runtime-persistence NON-LAW demonstration failed: {error}")
        });
    replay_regression_corpus(&make)
        .await
        .unwrap_or_else(|error| panic!("{backend} runtime-persistence regression failed: {error}"));

    let runtime = tokio::runtime::Handle::current();
    let totals = Arc::new(RunShapeTotals::default());
    let runner_totals = Arc::clone(&totals);
    let config = Config {
        cases,
        max_shrink_iters: 8_192,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(runner_seed),
        ..Config::default()
    };
    let result = tokio::task::spawn_blocking(move || {
        let mut runner = TestRunner::new(config);
        runner.run(&generated_case(), |case| {
            runtime.block_on(async {
                let shape = replay_case(make(case.seed).await, case.seed, &case.operations).await?;
                assert_required_shape(shape)?;
                runner_totals.add(shape);
                Ok(())
            })
        })
    })
    .await
    .expect("runtime-persistence property runner task");

    if let Err(error) = result {
        persist_counterexample(backend, runner_seed, &error);
        panic!(
            "{backend} runtime-persistence property law failed with runner seed {runner_seed}; replay with LASH_RUNTIME_PERSISTENCE_PROPTEST_SEED={runner_seed}: {error}"
        );
    }

    eprintln!(
        "runtime-persistence run shape ({backend}, cases={cases}): {}",
        totals.report()
    );
}

fn assert_required_shape(shape: RunShape) -> Result<(), TestCaseError> {
    for counter in RunShapeCounter::ALL {
        prop_assert!(
            shape[*counter] > 0,
            "generated alphabet starvation: no {}",
            counter.name()
        );
    }
    Ok(())
}

async fn replay_case(
    handles: RuntimePersistenceStateMachineHandles,
    seed: u64,
    operations: &[RuntimePersistenceOp],
) -> Result<RunShape, TestCaseError> {
    let mut model = ReferenceModel::default();
    let mut shape = RunShape::default();
    for (step, operation) in operations.iter().enumerate() {
        apply_operation(
            handles.runtime.as_ref(),
            Some(&handles),
            &mut model,
            &mut shape,
            seed,
            operation,
        )
        .await
        .map_err(|reason| TestCaseError::fail(format!("step {step} {operation:?}: {reason}")))?;
        assert_model_agreement(handles.runtime.as_ref(), &model)
            .await
            .map_err(|reason| {
                TestCaseError::fail(format!("model agreement at step {step}: {reason}"))
            })?;
        assert_usage_conservation(handles.runtime.as_ref(), &model)
            .await
            .map_err(|reason| {
                TestCaseError::fail(format!("usage conservation at step {step}: {reason}"))
            })?;
        assert_attachment_conservation(&handles, &mut model)
            .await
            .map_err(|reason| {
                TestCaseError::fail(format!("attachment conservation at step {step}: {reason}"))
            })?;
    }
    Ok(shape)
}

async fn replay_regression_corpus<F, Fut>(make: &F) -> Result<(), TestCaseError>
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = RuntimePersistenceStateMachineHandles>,
{
    let cases: Vec<GeneratedCase> = serde_json::from_str(include_str!(
        "runtime_persistence_state_machine_regressions.json"
    ))
    .map_err(|error| TestCaseError::fail(format!("invalid regression corpus: {error}")))?;
    for (index, case) in cases.iter().enumerate() {
        replay_case(make(case.seed).await, case.seed, &case.operations)
            .await
            .map_err(|error| TestCaseError::fail(format!("regression case {index}: {error}")))?;
    }
    Ok(())
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn apply_operation(
    store: &dyn RuntimePersistence,
    attachment_handles: Option<&RuntimePersistenceStateMachineHandles>,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    seed: u64,
    operation: &RuntimePersistenceOp,
) -> Result<(), String> {
    use RuntimePersistenceOp::*;
    match operation {
        ClaimLease { owner } => claim_lease(store, model, shape, *owner).await?,
        RenewLease { stale } => renew_lease(store, model, shape, *stale).await?,
        Crash => crash_between_claim_and_commit(store, model, shape).await?,
        EnqueueWork {
            slot,
            value,
            coalesce,
        } => {
            let draft = queued_draft(*slot, *value, *coalesce);
            let key = draft.source_key.clone().expect("property source key");
            let result = store.enqueue_queued_work(draft.clone()).await;
            match model.work.get(&key) {
                Some(existing) => {
                    let replay = result.map_err(|error| error.to_string())?;
                    if replay.batch_id != existing.batch.batch_id {
                        return Err("queue source-key replay minted a different batch".to_string());
                    }
                }
                None => {
                    let batch = result.map_err(|error| error.to_string())?;
                    if batch.enqueue_seq == 0 {
                        return Err("fresh queued work returned a consumed receipt".to_string());
                    }
                    model.work.insert(key, ModeledWork { batch });
                    shape[RunShapeCounter::QueueEnqueues] += 1;
                }
            }
        }
        ClaimWork {
            selected,
            selection,
        } => {
            let Some(lease) = model.current_lease.as_ref() else {
                return Ok(());
            };
            let pending = pending_work(model);
            if pending.is_empty() {
                return Ok(());
            }
            let claim = if *selected {
                let batch_id = pending[usize::from(*selection) % pending.len()]
                    .batch_id
                    .clone();
                let required_composition = interrupted_work_composition(model, &batch_id)
                    .filter(|required| required.len() > 1);
                let before = required_composition
                    .as_ref()
                    .map(|_| session_snapshot(store));
                let result = store
                    .claim_ready_queued_work_by_batch_ids(
                        &session_id(),
                        &lease.fence(),
                        &lease.owner,
                        QueuedWorkClaimBoundary::Idle,
                        std::slice::from_ref(&batch_id),
                        crate::testing::queued_work_claim_policy(64),
                    )
                    .await;
                if let Some(required_batch_ids) = required_composition {
                    if !matches!(
                        &result,
                        Err(StoreError::SelectedQueuedWorkRequiresInterruptedComposition {
                            required_batch_ids: actual,
                        }) if actual == &required_batch_ids
                    ) {
                        return Err(format!(
                            "partial interrupted-composition selection was not refused with its literal composition {required_batch_ids:?}: {result:?}"
                        ));
                    }
                    assert_snapshot_unchanged(
                        store,
                        before
                            .expect("interrupted-composition refusal captured a snapshot")
                            .await?,
                        "partial interrupted-composition selected claim",
                    )
                    .await?;
                    None
                } else {
                    result
                        .map_err(|error| error.to_string())?
                        .map(|claim| (claim, Some(batch_id)))
                }
            } else {
                store
                    .claim_ready_queued_work(
                        &session_id(),
                        &lease.fence(),
                        &lease.owner,
                        QueuedWorkClaimBoundary::Idle,
                        crate::testing::queued_work_claim_policy(4),
                    )
                    .await
                    .map(crate::QueuedWorkClaimOutcome::claim)
                    .map_err(|error| error.to_string())?
                    .map(|claim| (claim, None))
            };
            if let Some((claim, selected_id)) = claim {
                validate_work_claim(model, lease, &claim, selected_id.as_deref())?;
                if selected_id.is_some() {
                    shape[RunShapeCounter::SelectedBatchClaims] += 1;
                }
                if claim.batches.len() > 1 {
                    shape[RunShapeCounter::CoalescedClaims] += 1;
                }
                for batch in &claim.batches {
                    if model.crashed_work.remove(&batch.batch_id) {
                        shape[RunShapeCounter::CrashReclaims] += 1;
                    }
                }
                shape[RunShapeCounter::QueueClaims] += 1;
                model.active_work_claims.push(claim);
            }
        }
        ClaimWorkWithStaleLease => {
            claim_work_with_stale_lease(store, model, shape).await?;
        }
        CancelWork { selection } => {
            let Some(work) = select_modeled_work(model, *selection) else {
                return Ok(());
            };
            let held = active_work_ids(model).contains(&work.batch.batch_id);
            let removed = store
                .cancel_queued_work_batch(&session_id(), &work.batch.batch_id)
                .await
                .map_err(|error| error.to_string())?;
            if held && removed.is_some() {
                return Err("cancel removed work held by a live claim".to_string());
            }
            if let Some(removed) = removed {
                if removed.batch_id != work.batch.batch_id {
                    return Err("queue cancel returned a different batch".to_string());
                }
                model
                    .work
                    .retain(|_, candidate| candidate.batch.batch_id != removed.batch_id);
                shape[RunShapeCounter::QueueCancellations] += 1;
            }
        }
        EnqueueTurnInput { slot, value } => {
            let draft = turn_input_draft(*slot, *value);
            let key = draft.source_key.clone().expect("property source key");
            let result = store.enqueue_pending_turn_input(draft.clone()).await;
            match model.input_receipts.get(&key) {
                Some(existing) if json(existing)? != json(&draft)? => {
                    if result.is_ok() {
                        return Err(
                            "turn-input source-key conflict accepted different content".to_string()
                        );
                    }
                }
                Some(_) => {
                    let replay = result.map_err(|error| error.to_string())?;
                    if let Some(existing) = model.inputs.get(&key)
                        && replay.input_id != existing.input.input_id
                    {
                        return Err(
                            "turn-input source-key replay minted a different input".to_string()
                        );
                    }
                    if !model.inputs.contains_key(&key)
                        && matches!(
                            replay.state.kind(),
                            crate::TurnInputStateKind::PendingActive
                                | crate::TurnInputStateKind::DeferredNextTurn
                        )
                    {
                        return Err("terminal turn-input replay became pending again".to_string());
                    }
                }
                None => {
                    let input = result.map_err(|error| error.to_string())?;
                    if !matches!(
                        input.state.kind(),
                        crate::TurnInputStateKind::PendingActive
                            | crate::TurnInputStateKind::DeferredNextTurn
                    ) {
                        return Err("fresh turn input returned a terminal receipt".to_string());
                    }
                    model.input_receipts.insert(key.clone(), draft.clone());
                    model.inputs.insert(key, ModeledInput { input });
                    shape[RunShapeCounter::InputEnqueues] += 1;
                }
            }
        }
        ClaimTurnInputs { max_inputs } => {
            let Some(lease) = model.current_lease.as_ref() else {
                return Ok(());
            };
            let expected = pending_inputs(model);
            if expected.is_empty() {
                return Ok(());
            }
            if let Some(claim) = store
                .claim_next_turn_inputs(
                    &session_id(),
                    &lease.fence(),
                    &lease.owner,
                    usize::from((*max_inputs).max(1)),
                )
                .await
                .map_err(|error| error.to_string())?
            {
                validate_input_claim(lease, &claim, &expected, usize::from((*max_inputs).max(1)))?;
                for input in &claim.inputs {
                    if model.crashed_inputs.remove(&input.input_id) {
                        shape[RunShapeCounter::CrashReclaims] += 1;
                    }
                }
                shape[RunShapeCounter::InputClaims] += 1;
                model.active_input_claims.push(claim);
            }
        }
        ClaimTurnInputsWithStaleLease => {
            claim_turn_inputs_with_stale_lease(store, model, shape).await?;
        }
        CancelTurnInput { selection } => {
            let Some(input) = select_modeled_input(model, *selection) else {
                return Ok(());
            };
            let held = active_input_ids(model).contains(&input.input.input_id);
            let outcome = store
                .cancel_pending_turn_input(&session_id(), &input.input.input_id)
                .await
                .map_err(|error| error.to_string())?;
            match outcome {
                PendingTurnInputCancelOutcome::Cancelled(cancelled) => {
                    if held {
                        return Err("cancel removed input held by a live claim".to_string());
                    }
                    model
                        .inputs
                        .retain(|_, candidate| candidate.input.input_id != cancelled.input_id);
                    shape[RunShapeCounter::InputCancellations] += 1;
                }
                PendingTurnInputCancelOutcome::AlreadyClaimed { .. } if held => {}
                PendingTurnInputCancelOutcome::AlreadyClaimed { .. } => {
                    return Err(
                        "input remained claimed after its lease generation died".to_string()
                    );
                }
                other => {
                    return Err(format!(
                        "unexpected cancel outcome for live modeled input: {other:?}"
                    ));
                }
            }
        }
        RecordUsage { slot, value } => record_usage(model, shape, *slot, *value)?,
        StageUsage { replay_last_commit } => stage_usage(model, shape, seed, *replay_last_commit)?,
        ConfirmUsage { selection } => confirm_usage(model, shape, *selection)?,
        ReplayUsageReceipt => replay_usage_receipt(store, model, shape).await?,
        attachment_operation @ (CommitWithAttachmentRefs { .. }
        | PutAttachmentIntent { .. }
        | ReplayAttachmentCommit { .. }
        | ReclaimAttachmentSession { .. }
        | ProbeAttachmentGc) => {
            let handles = attachment_handles.ok_or_else(|| {
                "attachment operation requires factory and blob handles".to_string()
            })?;
            apply_attachment_operation(handles, model, shape, seed, attachment_operation).await?
        }
        Commit {
            component_mode,
            value,
            settle_work,
            settle_inputs,
            stale_head,
        } => {
            commit_operation(
                store,
                model,
                shape,
                seed,
                *component_mode,
                *value,
                *settle_work,
                *settle_inputs,
                *stale_head,
            )
            .await?;
        }
        SettleStaleWork => settle_stale_work(store, model, shape, seed).await?,
        SettleStaleTurnInputs => settle_stale_input(store, model, shape, seed).await?,
    }
    Ok(())
}

async fn claim_work_with_stale_lease(
    store: &dyn RuntimePersistence,
    model: &ReferenceModel,
    shape: &mut RunShape,
) -> Result<(), String> {
    let (Some(stale), Some(batch)) = (
        model.stale_leases.last(),
        pending_work(model).first().cloned(),
    ) else {
        return Ok(());
    };
    let before = session_snapshot(store).await?;
    let result = store
        .claim_ready_queued_work_by_batch_ids(
            &session_id(),
            &stale.fence(),
            &stale.owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&batch.batch_id),
            crate::testing::queued_work_claim_policy(64),
        )
        .await;
    if !matches!(result, Err(StoreError::SessionExecutionLeaseExpired { .. })) {
        return Err(format!(
            "superseded lease generation claimed queued work: {result:?}"
        ));
    }
    assert_snapshot_unchanged(store, before, "superseded-generation queued-work claim").await?;
    shape[RunShapeCounter::LeaseFenceRejections] += 1;
    Ok(())
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn claim_turn_inputs_with_stale_lease(
    store: &dyn RuntimePersistence,
    model: &ReferenceModel,
    shape: &mut RunShape,
) -> Result<(), String> {
    if model.stale_leases.is_empty() || pending_inputs(model).is_empty() {
        return Ok(());
    }
    let stale = model.stale_leases.last().expect("checked stale lease");
    let before = session_snapshot(store).await?;
    let result = store
        .claim_next_turn_inputs(&session_id(), &stale.fence(), &stale.owner, 1)
        .await;
    if !matches!(result, Err(StoreError::SessionExecutionLeaseExpired { .. })) {
        return Err(format!(
            "superseded lease generation claimed turn inputs: {result:?}"
        ));
    }
    assert_snapshot_unchanged(store, before, "superseded-generation turn-input claim").await?;
    shape[RunShapeCounter::LeaseFenceRejections] += 1;
    Ok(())
}

async fn claim_lease(
    store: &dyn RuntimePersistence,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    owner_index: u8,
) -> Result<(), String> {
    let owner = owner(owner_index);
    let s = SESSION_ID;
    let e = format!("state-machine-executor-{owner_index}");
    let n = crate::LeaseClaimNonce::new();
    let outcome = store
        .try_claim_session_execution_lease_with_token(&SessionId::from(s), &owner, &e, &n, 60_000)
        .await
        .map_err(|error| error.to_string())?;
    match (&model.current_lease, outcome) {
        (Some(current), SessionExecutionLeaseClaimOutcome::Busy { holder }) => {
            if current.owner.same_incarnation(&owner) {
                return Err("same incarnation unexpectedly received Busy".to_string());
            }
            if holder.fencing_token != current.fencing_token {
                return Err("Busy reported a different live generation".to_string());
            }
        }
        (Some(current), SessionExecutionLeaseClaimOutcome::Acquired(acquisition)) => {
            if !current.owner.same_incarnation(&owner)
                || acquisition.lease.fencing_token != current.fencing_token
            {
                return Err("competing owner acquired an unexpired lease".to_string());
            }
            if acquisition.displaced.is_some() {
                return Err("same-incarnation reentry reported a displaced holder".to_string());
            }
            model.current_lease = Some(acquisition.lease);
        }
        (None, SessionExecutionLeaseClaimOutcome::Acquired(acquisition)) => {
            if model
                .stale_leases
                .last()
                .is_some_and(|stale| acquisition.lease.fencing_token <= stale.fencing_token)
            {
                return Err("successor lease did not advance the fencing generation".to_string());
            }
            if let Some(displaced) = acquisition.displaced.as_ref() {
                if displaced.fencing_token >= acquisition.lease.fencing_token {
                    return Err(
                        "displaced generation was not below the acquired generation".to_string()
                    );
                }
                if displaced.owner.same_incarnation(&owner) {
                    return Err("a claim reported displacing its own incarnation".to_string());
                }
            }
            model.current_lease = Some(acquisition.lease);
            shape[RunShapeCounter::LeaseAcquisitions] += 1;
        }
        (None, SessionExecutionLeaseClaimOutcome::Busy { .. }) => {
            return Err("released/absent lease remained busy".to_string());
        }
    }
    Ok(())
}

async fn renew_lease(
    store: &dyn RuntimePersistence,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    stale: bool,
) -> Result<(), String> {
    let lease = if stale {
        model.stale_leases.last()
    } else {
        model.current_lease.as_ref()
    };
    let Some(lease) = lease else {
        return Ok(());
    };
    let before = session_snapshot(store).await?;
    let result = store
        .renew_session_execution_lease(&lease.fence(), 60_000)
        .await;
    if stale {
        if !matches!(result, Err(RenewalRefused { .. })) {
            return Err(format!(
                "superseded lease renewal was not fenced: {result:?}"
            ));
        }
        assert_snapshot_unchanged(store, before, "superseded lease renewal").await?;
        shape[RunShapeCounter::LeaseFenceRejections] += 1;
    } else {
        let renewed = result.map_err(|error| error.to_string())?;
        if renewed.fencing_token != lease.fencing_token {
            return Err("renewal changed the fencing generation".to_string());
        }
        model.current_lease = Some(renewed);
    }
    Ok(())
}

async fn crash_between_claim_and_commit(
    store: &dyn RuntimePersistence,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
) -> Result<(), String> {
    let Some(lease) = model.current_lease.take() else {
        return Ok(());
    };
    store
        .release_session_execution_lease(&lease.completion())
        .await
        .map_err(|error| error.to_string())?;
    model.stale_leases.push(lease);
    for claim in model.active_work_claims.drain(..) {
        model
            .crashed_work
            .extend(claim.batches.iter().map(|batch| batch.batch_id.clone()));
        model.stale_work_claims.push(claim);
    }
    for claim in model.active_input_claims.drain(..) {
        model
            .crashed_inputs
            .extend(claim.inputs.iter().map(|input| input.input_id.clone()));
        model.stale_input_claims.push(claim);
    }
    shape[RunShapeCounter::CrashPoints] += 1;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn commit_operation(
    store: &dyn RuntimePersistence,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    seed: u64,
    component_mode: u8,
    value: u8,
    settle_work: bool,
    settle_inputs: bool,
    stale_head: bool,
) -> Result<(), String> {
    let mut state = modeled_state(model);
    install_component_bodies(&mut state, component_mode, value);
    let before_components = model.components.clone();
    let work_claim = settle_work
        .then(|| model.active_work_claims.first().cloned())
        .flatten();
    let mut input_claim = settle_inputs
        .then(|| model.active_input_claims.first().cloned())
        .flatten();
    if let Some(claim) = input_claim.as_mut() {
        claim.record_initial_turn_application(
            &crate::TurnId::from(format!("property-turn-{}", model.operation_sequence)),
            &format!("property-message-{}", model.operation_sequence),
        );
    }
    let expected_applications = input_claim
        .as_ref()
        .map(|claim| claim.applications.clone())
        .unwrap_or_default();
    let mut staged_usage = model.staged_usage.take();
    let mut staged_usage_operation = model.staged_usage_operation.take();
    let staged_replays_last_commit = match (
        staged_usage_operation.as_ref(),
        model.last_usage_commit.as_ref(),
    ) {
        (Some(staged), Some(last)) => {
            staged.storage_key().map_err(|error| error.to_string())?
                == last
                    .turn_commit
                    .operation
                    .storage_key()
                    .map_err(|error| error.to_string())?
        }
        _ => false,
    };
    if staged_replays_last_commit {
        model.staged_usage = staged_usage.take();
        model.staged_usage_operation = staged_usage_operation.take();
    }
    let submitted_usage = staged_usage
        .as_ref()
        .map(|staged| staged.deltas().to_vec())
        .unwrap_or_default();
    let operation = if let Some(operation) = staged_usage_operation.clone() {
        operation
    } else {
        model.operation_sequence += 1;
        crate::OperationId::new(
            crate::ExecutionScope::runtime_operation(format!(
                "runtime-persistence-property:{seed}:{}",
                model.operation_sequence
            )),
            "commit",
        )
    };
    let (mut commit, _) = RuntimeCommit::persisted_state_with_operation_and_staged_usage(
        &mut state,
        &submitted_usage,
        operation,
    )
    .map_err(|error| error.to_string())?;
    if stale_head {
        commit.expected_head_revision = model
            .head_revision
            .checked_add(1)
            .expect("generated model head revision must remain in range");
    }
    if let Some(claim) = &work_claim {
        commit = commit.completing_queue_claim(claim.completion());
    }
    if let Some(claim) = &input_claim {
        commit = commit.completing_turn_input_claim(claim.completion());
    }

    let before = session_snapshot(store).await?;
    let committed_envelope = commit.clone();
    let result = store.commit_runtime_state(commit).await;
    if stale_head {
        model.staged_usage = staged_usage;
        model.staged_usage_operation = staged_usage_operation;
        if !matches!(result, Err(StoreError::HeadRevisionConflict { .. })) {
            return Err(format!(
                "stale expected head was not rejected by HeadRevisionConflict: {result:?}"
            ));
        }
        assert_snapshot_unchanged(store, before, "stale expected-head rejection").await?;
        shape[RunShapeCounter::StaleHeadRejections] += 1;
        return Ok(());
    }

    let result = result.map_err(|error| error.to_string())?;
    if result.head_revision != model.head_revision + 1 {
        return Err(format!(
            "accepted commit advanced head {} -> {}",
            model.head_revision, result.head_revision
        ));
    }
    if result.turn_input_applications != expected_applications {
        return Err("commit returned different turn-input applications".to_string());
    }
    register_committed_usage(
        model,
        &submitted_usage,
        &result.committed_usage_delta_identities,
    )?;
    if let Some(staged) = staged_usage {
        model
            .pending_usage_confirmations
            .push(PendingUsageConfirmation {
                staged,
                identities: result.committed_usage_delta_identities.clone(),
            });
        model.last_usage_commit = Some(committed_envelope);
    }
    model.head_revision = result.head_revision;
    model.has_session = true;
    update_components_after_commit(
        model,
        &before_components,
        component_mode,
        value,
        &result.manifest,
        shape,
    )?;

    if let Some(claim) = work_claim {
        let pending_before = model
            .work
            .values()
            .map(|work| work.batch.enqueue_seq)
            .collect::<Vec<_>>();
        let settled = claim
            .batches
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<BTreeSet<_>>();
        let settled_min = claim
            .batches
            .iter()
            .map(|batch| batch.enqueue_seq)
            .min()
            .unwrap_or(0);
        if pending_before
            .iter()
            .any(|sequence| *sequence < settled_min)
        {
            shape[RunShapeCounter::OutOfOrderSettlements] += 1;
        }
        model
            .work
            .retain(|_, work| !settled.contains(work.batch.batch_id.as_str()));
        model.active_work_claims.remove(0);
        shape[RunShapeCounter::QueueCompletions] += claim.batches.len() as u64;
    }
    if let Some(claim) = input_claim {
        let settled = claim
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<BTreeSet<_>>();
        model
            .inputs
            .retain(|_, input| !settled.contains(input.input.input_id.as_str()));
        model.active_input_claims.remove(0);
        model.applications.extend(expected_applications);
        shape[RunShapeCounter::InputApplications] += claim.inputs.len() as u64;
    }
    shape[RunShapeCounter::AcceptedCommits] += 1;
    Ok(())
}

fn update_components_after_commit(
    model: &mut ReferenceModel,
    before: &ComponentModel,
    mode: u8,
    value: u8,
    manifest: &crate::SessionCheckpoint,
    shape: &mut RunShape,
) -> Result<(), String> {
    let selection = component_selection(mode);
    check_component_ref(
        "tool-state",
        selection.store_tool,
        before.tool_value,
        Some(value),
        before.tool_ref.as_ref(),
        manifest.component_ref(TOOL_STATE_CHECKPOINT_COMPONENT),
        shape,
    )?;
    check_component_ref(
        "plugin-state",
        selection.store_plugin,
        before.plugin_value,
        Some(value),
        before.plugin_ref.as_ref(),
        manifest.component_ref(PLUGIN_STATE_CHECKPOINT_COMPONENT),
        shape,
    )?;
    if selection.clear_execution {
        if manifest
            .component_ref(EXECUTION_STATE_CHECKPOINT_COMPONENT)
            .is_some()
        {
            return Err("cleared execution-state transition retained its ref".to_string());
        }
        if before.execution_ref.is_some() {
            shape[RunShapeCounter::CheckpointClears] += 1;
        }
    } else {
        check_component_ref(
            "execution-state",
            selection.store_execution,
            before.execution_value,
            Some(value),
            before.execution_ref.as_ref(),
            manifest.component_ref(EXECUTION_STATE_CHECKPOINT_COMPONENT),
            shape,
        )?;
    }
    if selection.store_tool {
        model.components.tool_value = Some(value);
    }
    if selection.store_plugin {
        model.components.plugin_value = Some(value);
    }
    if selection.clear_execution {
        model.components.execution_value = None;
    } else if selection.store_execution {
        model.components.execution_value = Some(value);
    }
    model.components.tool_ref = manifest
        .component_ref(TOOL_STATE_CHECKPOINT_COMPONENT)
        .cloned();
    model.components.plugin_ref = manifest
        .component_ref(PLUGIN_STATE_CHECKPOINT_COMPONENT)
        .cloned();
    model.components.execution_ref = manifest
        .component_ref(EXECUTION_STATE_CHECKPOINT_COMPONENT)
        .cloned();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn check_component_ref(
    name: &str,
    stored: bool,
    previous_value: Option<u8>,
    supplied_value: Option<u8>,
    previous_ref: Option<&crate::BlobRef>,
    actual_ref: Option<&crate::BlobRef>,
    shape: &mut RunShape,
) -> Result<(), String> {
    if stored {
        let actual_ref = actual_ref.ok_or_else(|| format!("stored {name} body returned no ref"))?;
        if previous_value == supplied_value {
            if let Some(previous_ref) = previous_ref
                && actual_ref != previous_ref
            {
                return Err(format!(
                    "unchanged {name} body did not reuse its content ref"
                ));
            }
        } else if previous_ref.is_some_and(|previous_ref| previous_ref == actual_ref) {
            return Err(format!("changed {name} body reused the old content ref"));
        }
        shape[RunShapeCounter::CheckpointStores] += 1;
    } else if actual_ref != previous_ref {
        return Err(format!(
            "ref-only {name} transition did not preserve its ref"
        ));
    } else if actual_ref.is_some() {
        shape[RunShapeCounter::CheckpointRefReuses] += 1;
    }
    Ok(())
}

async fn settle_stale_work(
    store: &dyn RuntimePersistence,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    seed: u64,
) -> Result<(), String> {
    let Some(claim) = model.stale_work_claims.last().cloned() else {
        return Ok(());
    };
    let completion = claim.completion();
    let active_ids = active_work_ids(model);
    let owns_all = claim.batches.iter().all(|batch| {
        model
            .work
            .values()
            .any(|work| work.batch.batch_id == batch.batch_id)
            && !active_ids.contains(&batch.batch_id)
    });
    let mut commit = fresh_commit(model, seed, "stale-work")?;
    commit = commit.completing_queue_claim(completion);
    let before = session_snapshot(store).await?;
    let result = store.commit_runtime_state(commit).await;
    if !owns_all {
        if !matches!(result, Err(StoreError::QueuedWorkClaimSuperseded { .. })) {
            return Err(format!(
                "reclaimed queued-work claim was not superseded: {result:?}"
            ));
        }
        assert_snapshot_unchanged(store, before, "reclaimed queued-work settlement").await?;
        shape[RunShapeCounter::ClaimSupersessionRejections] += 1;
        return Ok(());
    }

    let result = result.map_err(|error| error.to_string())?;
    if result.head_revision != model.head_revision + 1 {
        return Err(
            "accepted stale-generation settlement did not advance the head once".to_string(),
        );
    }
    let settled = claim
        .batches
        .iter()
        .map(|batch| batch.batch_id.as_str())
        .collect::<BTreeSet<_>>();
    model
        .work
        .retain(|_, work| !settled.contains(work.batch.batch_id.as_str()));
    model
        .crashed_work
        .retain(|batch_id| !settled.contains(batch_id.as_str()));
    model.stale_work_claims.pop();
    model.head_revision = result.head_revision;
    model.has_session = true;
    shape[RunShapeCounter::QueueCompletions] += claim.batches.len() as u64;
    shape[RunShapeCounter::StaleClaimSettlements] += 1;
    shape[RunShapeCounter::AcceptedCommits] += 1;
    Ok(())
}

async fn settle_stale_input(
    store: &dyn RuntimePersistence,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    seed: u64,
) -> Result<(), String> {
    let Some(claim) = model.stale_input_claims.last().cloned() else {
        return Ok(());
    };
    let completion = claim.completion();
    let active_ids = active_input_ids(model);
    let owns_all = claim.inputs.iter().all(|input| {
        model
            .inputs
            .values()
            .any(|modeled| modeled.input.input_id == input.input_id)
            && !active_ids.contains(&input.input_id)
    });
    let mut commit = fresh_commit(model, seed, "stale-input")?;
    commit = commit.completing_turn_input_claim(completion.clone());
    let before = session_snapshot(store).await?;
    let result = store.commit_runtime_state(commit).await;
    if !owns_all {
        if !matches!(result, Err(StoreError::TurnInputClaimSuperseded { .. })) {
            return Err(format!(
                "reclaimed turn-input claim was not superseded: {result:?}"
            ));
        }
        assert_snapshot_unchanged(store, before, "reclaimed turn-input settlement").await?;
        shape[RunShapeCounter::ClaimSupersessionRejections] += 1;
        return Ok(());
    }

    let result = result.map_err(|error| error.to_string())?;
    if result.head_revision != model.head_revision + 1 {
        return Err(
            "accepted stale-generation input settlement did not advance the head once".to_string(),
        );
    }
    if result.turn_input_applications != completion.applications {
        return Err(
            "accepted stale-generation input settlement returned wrong applications".to_string(),
        );
    }
    let settled = claim
        .inputs
        .iter()
        .map(|input| input.input_id.as_str())
        .collect::<BTreeSet<_>>();
    model
        .inputs
        .retain(|_, input| !settled.contains(input.input.input_id.as_str()));
    model
        .crashed_inputs
        .retain(|input_id| !settled.contains(input_id.as_str()));
    model.applications.extend(completion.data.applications);
    model.stale_input_claims.pop();
    model.head_revision = result.head_revision;
    model.has_session = true;
    shape[RunShapeCounter::InputApplications] += claim.inputs.len() as u64;
    shape[RunShapeCounter::StaleClaimSettlements] += 1;
    shape[RunShapeCounter::AcceptedCommits] += 1;
    Ok(())
}

fn fresh_commit(
    model: &mut ReferenceModel,
    seed: u64,
    label: &str,
) -> Result<RuntimeCommit, String> {
    let state = modeled_state(model);
    model.operation_sequence += 1;
    RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_operation(crate::OperationId::new(
            crate::ExecutionScope::runtime_operation(format!(
                "runtime-persistence-property:{seed}:{label}:{}",
                model.operation_sequence
            )),
            "commit",
        ))
        .map(|pair| pair.0)
        .map_err(|error| error.to_string())
}

fn modeled_state(model: &ReferenceModel) -> RuntimeSessionState {
    let mut state = RuntimeSessionState {
        session_id: session_id(),
        head_revision: model.head_revision,
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.checkpoint_components =
        lash_core::testing::conformance_support::RuntimeCheckpointComponents::complete_refs_for_testing(
            [
                (
                    TOOL_STATE_CHECKPOINT_COMPONENT.to_string(),
                    model.components.tool_ref.clone(),
                ),
                (
                    PLUGIN_STATE_CHECKPOINT_COMPONENT.to_string(),
                    model.components.plugin_ref.clone(),
                ),
                (
                    EXECUTION_STATE_CHECKPOINT_COMPONENT.to_string(),
                    model.components.execution_ref.clone(),
                ),
            ]
            .into_iter()
            .filter_map(|(key, blob_ref)| blob_ref.map(|blob_ref| (key, blob_ref))),
        );
    state
}

fn install_component_bodies(state: &mut RuntimeSessionState, mode: u8, value: u8) {
    let selection = component_selection(mode);
    if selection.store_tool {
        state.set_tool_state_snapshot(Some(
            ToolState::default().with_generation_for_conformance(u64::from(value)),
        ));
    }
    if selection.store_plugin {
        state.set_plugin_state(Some(plugin_state(value)));
    }
    if selection.clear_execution {
        state.set_execution_state_snapshot(None);
    } else if selection.store_execution {
        state.set_execution_state_snapshot(Some(vec![value, value.wrapping_add(1)]));
    }
}

fn owner(index: u8) -> LeaseOwnerIdentity {
    LeaseOwnerIdentity::opaque(
        format!("runtime-owner-{index}"),
        format!("incarnation-{index}"),
    )
}

fn queued_draft(slot: u8, value: u8, coalesce: bool) -> QueuedWorkBatchDraft {
    let draft = QueuedWorkBatchDraft::new(
        SESSION_ID,
        DeliveryPolicy::EarliestSafeBoundary,
        crate::TurnWorkPayload::agent_frame_task(
            crate::session_graph::frame_node_id(&session_id(), &format!("property-frame-{value}")),
            format!("property-work-{value}"),
            None,
        ),
    )
    .with_source_key(format!("runtime-property-work-{slot}"));
    if coalesce {
        draft.with_merge_key("runtime-property-coalesced")
    } else {
        draft
    }
}

fn turn_input_draft(slot: u8, value: u8) -> PendingTurnInputDraft {
    PendingTurnInputDraft::new(
        SESSION_ID,
        TurnInputIngress::next_turn(),
        TurnInput::text(format!("runtime property input {value}")),
    )
    .with_source_key(format!("runtime-property-input-{slot}"))
}

fn pending_work(model: &ReferenceModel) -> Vec<QueuedWorkBatch> {
    let held = active_work_ids(model);
    let mut work = model
        .work
        .values()
        .filter(|work| !held.contains(&work.batch.batch_id))
        .map(|work| work.batch.clone())
        .collect::<Vec<_>>();
    work.sort_by_key(|batch| batch.enqueue_seq);
    work
}

fn interrupted_work_composition(
    model: &ReferenceModel,
    selected_batch_id: &str,
) -> Option<Vec<lash_core::BatchId>> {
    let pending = pending_work(model)
        .into_iter()
        .map(|batch| batch.batch_id)
        .collect::<BTreeSet<_>>();
    model
        .stale_work_claims
        .iter()
        .rev()
        .find(|claim| {
            claim
                .batches
                .iter()
                .any(|batch| batch.batch_id == selected_batch_id)
        })
        .map(|claim| {
            claim
                .batches
                .iter()
                .filter(|batch| pending.contains(&batch.batch_id))
                .map(|batch| batch.batch_id.clone())
                .collect()
        })
}

fn pending_inputs(model: &ReferenceModel) -> Vec<PendingTurnInput> {
    let held = active_input_ids(model);
    let mut inputs = model
        .inputs
        .values()
        .filter(|input| !held.contains(&input.input.input_id))
        .map(|input| input.input.clone())
        .collect::<Vec<_>>();
    inputs.sort_by_key(|input| input.enqueue_seq);
    inputs
}

fn active_work_ids(model: &ReferenceModel) -> BTreeSet<lash_core::BatchId> {
    model
        .active_work_claims
        .iter()
        .flat_map(|claim| claim.batches.iter().map(|batch| batch.batch_id.clone()))
        .collect()
}

fn active_input_ids(model: &ReferenceModel) -> BTreeSet<lash_core::InputId> {
    model
        .active_input_claims
        .iter()
        .flat_map(|claim| claim.inputs.iter().map(|input| input.input_id.clone()))
        .collect()
}

fn select_modeled_work(model: &ReferenceModel, selection: u8) -> Option<ModeledWork> {
    let values = model.work.values().cloned().collect::<Vec<_>>();
    values
        .get(usize::from(selection) % values.len().max(1))
        .cloned()
}

fn select_modeled_input(model: &ReferenceModel, selection: u8) -> Option<ModeledInput> {
    let values = model.inputs.values().cloned().collect::<Vec<_>>();
    values
        .get(usize::from(selection) % values.len().max(1))
        .cloned()
}

fn validate_work_claim(
    model: &ReferenceModel,
    lease: &SessionExecutionLease,
    claim: &QueuedWorkClaim,
    selected_id: Option<&str>,
) -> Result<(), String> {
    if claim.session_lease_generation != lease.fencing_token {
        return Err("queued-work claim pinned the wrong lease generation".to_string());
    }
    let pending = pending_work(model)
        .into_iter()
        .map(|batch| batch.batch_id)
        .collect::<BTreeSet<_>>();
    let claimed = claim
        .batches
        .iter()
        .map(|batch| batch.batch_id.clone())
        .collect::<BTreeSet<_>>();
    if claimed.len() != claim.batches.len() || !claimed.is_subset(&pending) {
        return Err("queued-work claim duplicated or invented a batch".to_string());
    }
    if let Some(selected_id) = selected_id
        && claimed != BTreeSet::from([lash_core::BatchId::from(selected_id)])
    {
        return Err("selected-batch drain did not claim exactly the selected id".to_string());
    }
    Ok(())
}

fn validate_input_claim(
    lease: &SessionExecutionLease,
    claim: &TurnInputClaim,
    expected: &[PendingTurnInput],
    max_inputs: usize,
) -> Result<(), String> {
    if claim.session_lease_generation != lease.fencing_token {
        return Err("turn-input claim pinned the wrong lease generation".to_string());
    }
    let expected_ids = expected
        .iter()
        .take(max_inputs)
        .map(|input| input.input_id.as_str())
        .collect::<Vec<_>>();
    let actual_ids = claim
        .inputs
        .iter()
        .map(|input| input.input_id.as_str())
        .collect::<Vec<_>>();
    if actual_ids != expected_ids {
        return Err(format!(
            "turn inputs were not claimed once in enqueue order: actual={actual_ids:?} expected={expected_ids:?}"
        ));
    }
    Ok(())
}

/// Enforce queue-depth conservation through stronger element-wise agreement on
/// both queue read seams. Exact equality for total and pending queued work
/// subsumes a separate cardinality law, while the model's active claims define
/// the claimed remainder.
async fn assert_model_agreement(
    store: &dyn RuntimePersistence,
    model: &ReferenceModel,
) -> Result<(), String> {
    let mut actual_work = store
        .list_queued_work(&session_id())
        .await
        .map_err(|error| error.to_string())?;
    actual_work.sort_by_key(|batch| batch.enqueue_seq);
    let mut expected_work = model
        .work
        .values()
        .map(|work| work.batch.clone())
        .collect::<Vec<_>>();
    expected_work.sort_by_key(|batch| batch.enqueue_seq);
    if json(&actual_work)? != json(&expected_work)? {
        return Err("queued-work state differs from the reference model".to_string());
    }

    let actual_pending = store
        .list_pending_queued_work(&session_id())
        .await
        .map_err(|error| error.to_string())?;
    if json(&actual_pending)? != json(&pending_work(model))? {
        return Err("pending queued-work projection differs from live-claim model".to_string());
    }

    let actual_inputs = store
        .list_pending_turn_inputs(&session_id())
        .await
        .map_err(|error| error.to_string())?;
    if json(&actual_inputs)? != json(&pending_input_read_model::pending_input_reads(model))? {
        return Err(
            "pending turn-input projection differs from lifecycle and live-claim model".to_string(),
        );
    }
    let applications = store
        .list_turn_input_applications(&session_id())
        .await
        .map_err(|error| error.to_string())?;
    if applications != model.applications {
        return Err("turn-input applications differ from exactly-once order model".to_string());
    }

    let loaded = store
        .load_session()
        .await
        .map_err(|error| error.to_string())?;
    if !model.has_session {
        if loaded.is_some() {
            return Err("rejected/non-commit operations materialized a session head".to_string());
        }
        return Ok(());
    }
    let loaded = loaded.ok_or_else(|| "modeled session head disappeared".to_string())?;
    if loaded.head_revision != model.head_revision {
        return Err("head revision differs from the reference model".to_string());
    }
    let checkpoint = loaded
        .checkpoint
        .ok_or_else(|| "committed checkpoint did not hydrate".to_string())?;
    if checkpoint.component_ref(TOOL_STATE_CHECKPOINT_COMPONENT)
        != model.components.tool_ref.as_ref()
        || checkpoint.component_ref(PLUGIN_STATE_CHECKPOINT_COMPONENT)
            != model.components.plugin_ref.as_ref()
        || checkpoint.component_ref(EXECUTION_STATE_CHECKPOINT_COMPONENT)
            != model.components.execution_ref.as_ref()
    {
        return Err("checkpoint component refs differ from the reference model".to_string());
    }
    if checkpoint
        .decode_component::<ToolState>(TOOL_STATE_CHECKPOINT_COMPONENT)
        .map_err(|error| error.to_string())?
        .as_ref()
        .map(ToolState::generation)
        != model.components.tool_value.map(u64::from)
    {
        return Err("hydrated tool-state body differs from the reference model".to_string());
    }
    if checkpoint
        .decode_component::<PluginState>(PLUGIN_STATE_CHECKPOINT_COMPONENT)
        .map_err(|error| error.to_string())?
        .as_ref()
        .map(json)
        .transpose()?
        != model
            .components
            .plugin_value
            .map(plugin_state)
            .as_ref()
            .map(json)
            .transpose()?
    {
        return Err("hydrated plugin-state body differs from the reference model".to_string());
    }
    if checkpoint
        .component_body(EXECUTION_STATE_CHECKPOINT_COMPONENT)
        .map(<[u8]>::to_vec)
        != model
            .components
            .execution_value
            .map(|value| vec![value, value.wrapping_add(1)])
    {
        return Err("hydrated execution-state body differs from the reference model".to_string());
    }
    Ok(())
}

async fn session_snapshot(store: &dyn RuntimePersistence) -> Result<serde_json::Value, String> {
    let loaded = store
        .load_session()
        .await
        .map_err(|error| error.to_string())?;
    let head = loaded.map(|loaded| {
        let checkpoint = loaded.checkpoint.map(|checkpoint| {
            serde_json::json!({
                "components": checkpoint.components,
            })
        });
        serde_json::json!({
            "head_revision": loaded.head_revision,
            "current_frame_node_id": loaded.current_frame_node_id,
            "graph": loaded.graph,
            "checkpoint_ref": loaded.checkpoint_ref,
            "checkpoint": checkpoint,
            "token_ledger": loaded.token_ledger,
        })
    });
    Ok(serde_json::json!({
        "head": head,
        "work": store.list_queued_work(&session_id()).await.map_err(|error| error.to_string())?,
        "pending_work": store.list_pending_queued_work(&session_id()).await.map_err(|error| error.to_string())?,
        "pending_inputs": store.list_pending_turn_inputs(&session_id()).await.map_err(|error| error.to_string())?,
        "applications": store.list_turn_input_applications(&session_id()).await.map_err(|error| error.to_string())?,
    }))
}

async fn assert_snapshot_unchanged(
    store: &dyn RuntimePersistence,
    before: serde_json::Value,
    law: &str,
) -> Result<(), String> {
    let after = session_snapshot(store).await?;
    if after != before {
        return Err(format!("{law} mutated durable session state"));
    }
    Ok(())
}

fn json<T: serde::Serialize>(value: &T) -> Result<serde_json::Value, String> {
    serde_json::to_value(value).map_err(|error| error.to_string())
}
