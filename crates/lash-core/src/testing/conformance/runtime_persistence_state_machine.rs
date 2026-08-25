//! Model-based [`RuntimePersistence`] laws for leases, queues, inputs, commit
//! CAS, and checkpoint components; process-scoped laws live in the sibling harness.

use super::*;
use crate::StoreError::SessionExecutionLeaseRenewalRefused as RenewalRefused;
use crate::store::{
    EXECUTION_STATE_CHECKPOINT_COMPONENT, PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT,
    TOOL_STATE_CHECKPOINT_COMPONENT,
};
use crate::{
    LeaseOwnerIdentity, PendingTurnInput, PendingTurnInputCancelOutcome, PendingTurnInputDraft,
    PluginSessionSnapshot, PluginSnapshotEntry, PluginSnapshotMeta, QueuedWorkBatch,
    QueuedWorkBatchDraft, QueuedWorkClaim, QueuedWorkClaimBoundary, QueuedWorkPayload,
    RuntimeCommit, RuntimePersistence, RuntimeSessionState, RuntimeUsageDeltaIdentity,
    SessionExecutionLease, SessionExecutionLeaseClaimOutcome, StoreError, ToolState, TurnInput,
    TurnInputClaim, TurnInputIngress, facade_support::ToolStateFacadeOps,
};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
mod attachment_conservation;
mod claim_honesty;
mod counterexample;
mod generator;
mod interrupted_claim_laws;
#[cfg(test)]
mod tests;
mod usage_conservation;
pub use attachment_conservation::RuntimePersistenceStateMachineHandles;
use attachment_conservation::{apply_attachment_operation, assert_attachment_conservation};
use counterexample::persist_counterexample;
use generator::{component_selection, generated_case, plugin_snapshot};
use usage_conservation::{
    assert_usage_conservation, confirm_usage, record_usage, register_committed_usage,
    replay_usage_receipt, stage_usage,
};
const SESSION_ID: &str = "runtime-persistence-property";
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
    crashed_work: BTreeSet<String>,
    crashed_inputs: BTreeSet<String>,
    pending_usage: Arc<std::sync::Mutex<Vec<crate::runtime::PendingTokenLedgerEntry>>>,
    staged_usage: Option<crate::runtime::StagedTokenLedger>,
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
    staged: crate::runtime::StagedTokenLedger,
    identities: Vec<RuntimeUsageDeltaIdentity>,
}
#[derive(Clone, Copy, Debug, Default)]
struct RunShape {
    lease_acquisitions: u64,
    lease_fence_rejections: u64,
    queue_enqueues: u64,
    queue_claims: u64,
    selected_batch_claims: u64,
    queue_completions: u64,
    claim_supersession_rejections: u64,
    stale_claim_settlements: u64,
    out_of_order_settlements: u64,
    coalesced_claims: u64,
    queue_cancellations: u64,
    input_enqueues: u64,
    input_claims: u64,
    input_applications: u64,
    input_cancellations: u64,
    usage_records: u64,
    usage_stages: u64,
    usage_confirmations: u64,
    usage_receipt_replays: u64,
    attachment_commits: u64,
    attachment_intent_puts: u64,
    attachment_receipt_replays: u64,
    attachment_session_reclaims: u64,
    attachment_gc_probes: u64,
    accepted_commits: u64,
    stale_head_rejections: u64,
    checkpoint_stores: u64,
    checkpoint_ref_reuses: u64,
    checkpoint_clears: u64,
    crash_points: u64,
    crash_reclaims: u64,
}

#[derive(Debug, Default)]
struct RunShapeTotals {
    lease_acquisitions: AtomicU64,
    lease_fence_rejections: AtomicU64,
    queue_enqueues: AtomicU64,
    queue_claims: AtomicU64,
    selected_batch_claims: AtomicU64,
    queue_completions: AtomicU64,
    claim_supersession_rejections: AtomicU64,
    stale_claim_settlements: AtomicU64,
    out_of_order_settlements: AtomicU64,
    coalesced_claims: AtomicU64,
    queue_cancellations: AtomicU64,
    input_enqueues: AtomicU64,
    input_claims: AtomicU64,
    input_applications: AtomicU64,
    input_cancellations: AtomicU64,
    usage_records: AtomicU64,
    usage_stages: AtomicU64,
    usage_confirmations: AtomicU64,
    usage_receipt_replays: AtomicU64,
    attachment_commits: AtomicU64,
    attachment_intent_puts: AtomicU64,
    attachment_receipt_replays: AtomicU64,
    attachment_session_reclaims: AtomicU64,
    attachment_gc_probes: AtomicU64,
    accepted_commits: AtomicU64,
    stale_head_rejections: AtomicU64,
    checkpoint_stores: AtomicU64,
    checkpoint_ref_reuses: AtomicU64,
    checkpoint_clears: AtomicU64,
    crash_points: AtomicU64,
    crash_reclaims: AtomicU64,
}

impl RunShapeTotals {
    fn add(&self, shape: RunShape) {
        macro_rules! add {
            ($field:ident) => {
                self.$field.fetch_add(shape.$field, Ordering::Relaxed);
            };
        }
        add!(lease_acquisitions);
        add!(lease_fence_rejections);
        add!(queue_enqueues);
        add!(queue_claims);
        add!(selected_batch_claims);
        add!(queue_completions);
        add!(claim_supersession_rejections);
        add!(stale_claim_settlements);
        add!(out_of_order_settlements);
        add!(coalesced_claims);
        add!(queue_cancellations);
        add!(input_enqueues);
        add!(input_claims);
        add!(input_applications);
        add!(input_cancellations);
        add!(usage_records);
        add!(usage_stages);
        add!(usage_confirmations);
        add!(usage_receipt_replays);
        add!(attachment_commits);
        add!(attachment_intent_puts);
        add!(attachment_receipt_replays);
        add!(attachment_session_reclaims);
        add!(attachment_gc_probes);
        add!(accepted_commits);
        add!(stale_head_rejections);
        add!(checkpoint_stores);
        add!(checkpoint_ref_reuses);
        add!(checkpoint_clears);
        add!(crash_points);
        add!(crash_reclaims);
    }
}

/// Run generated runtime-persistence laws with shrinking and persisted counterexamples.
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
        "runtime-persistence run shape ({backend}, cases={cases}): lease_acquisitions={} lease_fence_rejections={} queue_enqueues={} queue_claims={} selected_batch_claims={} queue_completions={} claim_supersession_rejections={} stale_claim_settlements={} out_of_order_settlements={} coalesced_claims={} queue_cancellations={} input_enqueues={} input_claims={} input_applications={} input_cancellations={} usage_records={} usage_stages={} usage_confirmations={} usage_receipt_replays={} attachment_commits={} attachment_intent_puts={} attachment_receipt_replays={} attachment_session_reclaims={} attachment_gc_probes={} accepted_commits={} stale_head_rejections={} checkpoint_stores={} checkpoint_ref_reuses={} checkpoint_clears={} crash_points={} crash_reclaims={}",
        totals.lease_acquisitions.load(Ordering::Relaxed),
        totals.lease_fence_rejections.load(Ordering::Relaxed),
        totals.queue_enqueues.load(Ordering::Relaxed),
        totals.queue_claims.load(Ordering::Relaxed),
        totals.selected_batch_claims.load(Ordering::Relaxed),
        totals.queue_completions.load(Ordering::Relaxed),
        totals.claim_supersession_rejections.load(Ordering::Relaxed),
        totals.stale_claim_settlements.load(Ordering::Relaxed),
        totals.out_of_order_settlements.load(Ordering::Relaxed),
        totals.coalesced_claims.load(Ordering::Relaxed),
        totals.queue_cancellations.load(Ordering::Relaxed),
        totals.input_enqueues.load(Ordering::Relaxed),
        totals.input_claims.load(Ordering::Relaxed),
        totals.input_applications.load(Ordering::Relaxed),
        totals.input_cancellations.load(Ordering::Relaxed),
        totals.usage_records.load(Ordering::Relaxed),
        totals.usage_stages.load(Ordering::Relaxed),
        totals.usage_confirmations.load(Ordering::Relaxed),
        totals.usage_receipt_replays.load(Ordering::Relaxed),
        totals.attachment_commits.load(Ordering::Relaxed),
        totals.attachment_intent_puts.load(Ordering::Relaxed),
        totals.attachment_receipt_replays.load(Ordering::Relaxed),
        totals.attachment_session_reclaims.load(Ordering::Relaxed),
        totals.attachment_gc_probes.load(Ordering::Relaxed),
        totals.accepted_commits.load(Ordering::Relaxed),
        totals.stale_head_rejections.load(Ordering::Relaxed),
        totals.checkpoint_stores.load(Ordering::Relaxed),
        totals.checkpoint_ref_reuses.load(Ordering::Relaxed),
        totals.checkpoint_clears.load(Ordering::Relaxed),
        totals.crash_points.load(Ordering::Relaxed),
        totals.crash_reclaims.load(Ordering::Relaxed),
    );
}

fn assert_required_shape(shape: RunShape) -> Result<(), TestCaseError> {
    let required = [
        (shape.lease_acquisitions, "lease acquisitions"),
        (shape.lease_fence_rejections, "lease fence rejections"),
        (shape.queue_enqueues, "queue enqueues"),
        (shape.queue_claims, "queue claims"),
        (shape.selected_batch_claims, "selected-batch claims"),
        (shape.queue_completions, "queue completions"),
        (
            shape.claim_supersession_rejections,
            "reclaim-mediated claim-supersession rejections",
        ),
        (
            shape.stale_claim_settlements,
            "accepted stale-generation claim settlements",
        ),
        (shape.out_of_order_settlements, "out-of-order settlements"),
        (shape.coalesced_claims, "coalesced claims"),
        (shape.queue_cancellations, "queue cancellations"),
        (shape.input_enqueues, "input enqueues"),
        (shape.input_claims, "input claims"),
        (shape.input_applications, "input applications"),
        (shape.input_cancellations, "input cancellations"),
        (shape.usage_records, "usage records"),
        (shape.usage_stages, "usage stages"),
        (shape.usage_confirmations, "usage confirmations"),
        (shape.usage_receipt_replays, "usage receipt replays"),
        (shape.attachment_commits, "attachment commits"),
        (shape.attachment_intent_puts, "attachment intent puts"),
        (
            shape.attachment_receipt_replays,
            "attachment receipt replays",
        ),
        (
            shape.attachment_session_reclaims,
            "attachment session reclaims",
        ),
        (shape.attachment_gc_probes, "attachment GC probes"),
        (shape.accepted_commits, "accepted commits"),
        (shape.stale_head_rejections, "stale-head rejections"),
        (shape.checkpoint_stores, "checkpoint stores"),
        (shape.checkpoint_ref_reuses, "checkpoint ref reuses"),
        (shape.checkpoint_clears, "checkpoint clears"),
        (shape.crash_points, "claim-to-commit crash points"),
        (shape.crash_reclaims, "post-crash reclaims"),
    ];
    for (count, name) in required {
        prop_assert!(count > 0, "generated alphabet starvation: no {name}");
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
                    shape.queue_enqueues += 1;
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
                        SESSION_ID,
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
                        SESSION_ID,
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
                    shape.selected_batch_claims += 1;
                }
                if claim.batches.len() > 1 {
                    shape.coalesced_claims += 1;
                }
                for batch in &claim.batches {
                    if model.crashed_work.remove(&batch.batch_id) {
                        shape.crash_reclaims += 1;
                    }
                }
                shape.queue_claims += 1;
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
                .cancel_queued_work_batch(SESSION_ID, &work.batch.batch_id)
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
                shape.queue_cancellations += 1;
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
                            replay.state,
                            crate::TurnInputState::PendingActive
                                | crate::TurnInputState::DeferredNextTurn
                        )
                    {
                        return Err("terminal turn-input replay became pending again".to_string());
                    }
                }
                None => {
                    let input = result.map_err(|error| error.to_string())?;
                    if !matches!(
                        input.state,
                        crate::TurnInputState::PendingActive
                            | crate::TurnInputState::DeferredNextTurn
                    ) {
                        return Err("fresh turn input returned a terminal receipt".to_string());
                    }
                    model.input_receipts.insert(key.clone(), draft.clone());
                    model.inputs.insert(key, ModeledInput { input });
                    shape.input_enqueues += 1;
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
                    SESSION_ID,
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
                        shape.crash_reclaims += 1;
                    }
                }
                shape.input_claims += 1;
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
                .cancel_pending_turn_input(SESSION_ID, &input.input.input_id)
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
                    shape.input_cancellations += 1;
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
            SESSION_ID,
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
    shape.lease_fence_rejections += 1;
    Ok(())
}

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
        .claim_next_turn_inputs(SESSION_ID, &stale.fence(), &stale.owner, 1)
        .await;
    if !matches!(result, Err(StoreError::SessionExecutionLeaseExpired { .. })) {
        return Err(format!(
            "superseded lease generation claimed turn inputs: {result:?}"
        ));
    }
    assert_snapshot_unchanged(store, before, "superseded-generation turn-input claim").await?;
    shape.lease_fence_rejections += 1;
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
        .try_claim_session_execution_lease_with_token(s, &owner, &e, &n, 60_000)
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
            shape.lease_acquisitions += 1;
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
        shape.lease_fence_rejections += 1;
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
    shape.crash_points += 1;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
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
        shape.stale_head_rejections += 1;
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
            shape.out_of_order_settlements += 1;
        }
        model
            .work
            .retain(|_, work| !settled.contains(work.batch.batch_id.as_str()));
        model.active_work_claims.remove(0);
        shape.queue_completions += claim.batches.len() as u64;
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
        shape.input_applications += claim.inputs.len() as u64;
    }
    shape.accepted_commits += 1;
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
        "plugin-snapshot",
        selection.store_plugin,
        before.plugin_value,
        Some(value),
        before.plugin_ref.as_ref(),
        manifest.component_ref(PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT),
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
            shape.checkpoint_clears += 1;
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
        .component_ref(PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT)
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
        shape.checkpoint_stores += 1;
    } else if actual_ref != previous_ref {
        return Err(format!(
            "ref-only {name} transition did not preserve its ref"
        ));
    } else if actual_ref.is_some() {
        shape.checkpoint_ref_reuses += 1;
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
        shape.claim_supersession_rejections += 1;
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
    shape.queue_completions += claim.batches.len() as u64;
    shape.stale_claim_settlements += 1;
    shape.accepted_commits += 1;
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
        shape.claim_supersession_rejections += 1;
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
    shape.input_applications += claim.inputs.len() as u64;
    shape.stale_claim_settlements += 1;
    shape.accepted_commits += 1;
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
        session_id: SESSION_ID.to_string(),
        head_revision: model.head_revision,
        plugin_snapshot_revision: model.components.plugin_value.map(u64::from),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.checkpoint_components =
        crate::runtime::state::RuntimeCheckpointComponents::complete_refs_for_testing(
            [
                (
                    TOOL_STATE_CHECKPOINT_COMPONENT.to_string(),
                    model.components.tool_ref.clone(),
                ),
                (
                    PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT.to_string(),
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
        state.set_tool_state_snapshot(Some(ToolState::default().with_generation(u64::from(value))));
    }
    if selection.store_plugin {
        state.plugin_snapshot_revision = Some(u64::from(value));
        state.set_plugin_snapshot(Some(plugin_snapshot(value)));
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
        vec![QueuedWorkPayload::agent_frame_task(
            crate::session_graph::frame_node_id(SESSION_ID, &format!("property-frame-{value}")),
            format!("property-work-{value}"),
            None,
        )],
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
) -> Option<Vec<String>> {
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

fn active_work_ids(model: &ReferenceModel) -> BTreeSet<String> {
    model
        .active_work_claims
        .iter()
        .flat_map(|claim| claim.batches.iter().map(|batch| batch.batch_id.clone()))
        .collect()
}

fn active_input_ids(model: &ReferenceModel) -> BTreeSet<String> {
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
        && claimed != BTreeSet::from([selected_id.to_string()])
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
        .list_queued_work(SESSION_ID)
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
        .list_pending_queued_work(SESSION_ID)
        .await
        .map_err(|error| error.to_string())?;
    if json(&actual_pending)? != json(&pending_work(model))? {
        return Err("pending queued-work projection differs from live-claim model".to_string());
    }

    let actual_inputs = store
        .list_pending_turn_inputs(SESSION_ID)
        .await
        .map_err(|error| error.to_string())?;
    if json(&actual_inputs)? != json(&pending_inputs(model))? {
        return Err("pending turn-input projection differs from lifecycle model".to_string());
    }
    let applications = store
        .list_turn_input_applications(SESSION_ID)
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
        || checkpoint.component_ref(PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT)
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
    if checkpoint.plugin_snapshot_revision != model.components.plugin_value.map(u64::from)
        || checkpoint
            .decode_component::<PluginSessionSnapshot>(PLUGIN_SNAPSHOT_CHECKPOINT_COMPONENT)
            .map_err(|error| error.to_string())?
            .as_ref()
            .map(json)
            .transpose()?
            != model
                .components
                .plugin_value
                .map(plugin_snapshot)
                .as_ref()
                .map(json)
                .transpose()?
    {
        return Err("hydrated plugin-snapshot body differs from the reference model".to_string());
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
                "plugin_snapshot_revision": checkpoint.plugin_snapshot_revision,
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
        "work": store.list_queued_work(SESSION_ID).await.map_err(|error| error.to_string())?,
        "pending_work": store.list_pending_queued_work(SESSION_ID).await.map_err(|error| error.to_string())?,
        "pending_inputs": store.list_pending_turn_inputs(SESSION_ID).await.map_err(|error| error.to_string())?,
        "applications": store.list_turn_input_applications(SESSION_ID).await.map_err(|error| error.to_string())?,
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

async fn assert_dedicated_laws<F, Fut>(make: &F, seed: u64) -> Result<(), TestCaseError>
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = RuntimePersistenceStateMachineHandles>,
{
    assert_on_fresh_store(make, seed, |store| async move {
        law_lease_exclusivity_and_claim_generation_fencing(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 1, |store| async move {
        law_claimed_work_settles_exactly_once(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 2, |store| async move {
        law_reclaim_mediates_supersession(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 3, |store| async move {
        claim_honesty::law_reclaimed_predecessor_rejection_survives_successor_head_advance(store)
            .await
    })
    .await?;
    assert_on_fresh_store(make, seed + 4, |store| async move {
        law_head_cas_serializes_competing_commits(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 5, |store| async move {
        interrupted_claim_laws::stale_settlement_cannot_damage_successor(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 6, |store| async move {
        law_selected_batch_out_of_order_never_loses_work(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 7, |store| async move {
        law_turn_inputs_apply_once_in_order(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 8, |store| async move {
        law_commit_atomicity_and_stale_head_non_mutation(store).await
    })
    .await?;
    assert_on_fresh_store(make, seed + 9, |store| async move {
        law_checkpoint_refs_track_content(store).await
    })
    .await
}

async fn assert_on_fresh_store<F, Fut, Law, LawFut>(
    make: &F,
    seed: u64,
    law: Law,
) -> Result<(), TestCaseError>
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = RuntimePersistenceStateMachineHandles>,
    Law: FnOnce(Arc<dyn RuntimePersistence>) -> LawFut,
    LawFut: Future<Output = Result<(), TestCaseError>>,
{
    // Structural guard: every dedicated law obtains its own backend here.
    law(make(seed).await.runtime).await
}

async fn law_lease_exclusivity_and_claim_generation_fencing(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let mut model = ReferenceModel::default();
    let mut shape = RunShape::default();
    let ops = [
        RuntimePersistenceOp::ClaimLease { owner: 0 },
        RuntimePersistenceOp::EnqueueWork {
            slot: 0,
            value: 0,
            coalesce: false,
        },
        RuntimePersistenceOp::EnqueueTurnInput { slot: 0, value: 0 },
        RuntimePersistenceOp::ClaimLease { owner: 1 },
        RuntimePersistenceOp::Crash,
        RuntimePersistenceOp::ClaimLease { owner: 1 },
        RuntimePersistenceOp::RenewLease { stale: true },
        RuntimePersistenceOp::ClaimWorkWithStaleLease,
        RuntimePersistenceOp::ClaimTurnInputsWithStaleLease,
    ];
    for op in &ops {
        apply_operation(store.as_ref(), None, &mut model, &mut shape, 10, op)
            .await
            .map_err(TestCaseError::fail)?;
    }
    prop_assert!(
        shape.lease_fence_rejections >= 3,
        "generation fencing did not reject stale renewal and claim attempts"
    );
    prop_assert_eq!(model.work.len(), 1, "stale claim attempt removed work");
    prop_assert_eq!(model.inputs.len(), 1, "stale claim attempt removed input");
    Ok(())
}

async fn law_claimed_work_settles_exactly_once(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let batch = store
        .enqueue_queued_work(queued_draft(0, 0, false))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let owner = owner(0);
    let lease = store
        .try_claim_session_execution_lease(SESSION_ID, &owner, "claimed-work-executor", 60_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("lease busy"))?;
    let claim = store
        .claim_ready_queued_work_by_batch_ids(
            SESSION_ID,
            &lease.fence(),
            &owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&batch.batch_id),
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("selected work absent"))?;
    let mut state = RuntimeSessionState {
        session_id: SESSION_ID.to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let commit = RuntimeCommit::persisted_state_for_test(&state, &[])
        .completing_queue_claim(claim.completion());
    let first = store
        .commit_runtime_state(commit.clone())
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let replay = store
        .commit_runtime_state(commit)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(
        replay.head_revision,
        first.head_revision,
        "exact commit replay advanced the head twice"
    );
    prop_assert_eq!(
        &replay.checkpoint_ref,
        &first.checkpoint_ref,
        "exact commit replay returned a different receipt"
    );
    prop_assert!(
        store
            .list_queued_work(SESSION_ID)
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?
            .is_empty(),
        "settled work remained live"
    );
    state.apply_persisted_commit_result(first);
    let (second_settlement, _) = RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_operation(crate::OperationId::new(
            crate::ExecutionScope::runtime_operation("runtime-persistence-law:second-settlement"),
            "commit",
        ))
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let before = session_snapshot(store.as_ref())
        .await
        .map_err(TestCaseError::fail)?;
    let second = store
        .commit_runtime_state(second_settlement.completing_queue_claim(claim.completion()))
        .await;
    prop_assert!(
        matches!(second, Err(StoreError::QueuedWorkClaimSuperseded { .. })),
        "distinct second settlement was not rejected: {second:?}"
    );
    assert_snapshot_unchanged(store.as_ref(), before, "distinct second settlement")
        .await
        .map_err(TestCaseError::fail)?;
    Ok(())
}

async fn law_reclaim_mediates_supersession(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let first = store
        .enqueue_queued_work(queued_draft(0, 0, true))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let second = store
        .enqueue_queued_work(queued_draft(1, 1, true))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let stale_owner = owner(0);
    let stale_lease = store
        .try_claim_session_execution_lease(
            SESSION_ID,
            &stale_owner,
            "reclaim-stale-executor",
            60_000,
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("stale-owner lease busy"))?;
    let stale_claim = store
        .claim_ready_queued_work(
            SESSION_ID,
            &stale_lease.fence(),
            &stale_owner,
            QueuedWorkClaimBoundary::Idle,
            crate::testing::queued_work_claim_policy(4),
        )
        .await
        .map(crate::QueuedWorkClaimOutcome::claim)
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("coalesced work absent"))?;
    prop_assert_eq!(stale_claim.batches.len(), 2, "join claim did not coalesce");
    store
        .release_session_execution_lease(&stale_lease.completion())
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;

    let successor_owner = owner(1);
    let successor_lease = store
        .try_claim_session_execution_lease(
            SESSION_ID,
            &successor_owner,
            "reclaim-successor-executor",
            60_000,
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("successor lease busy"))?;
    let before_partial_selection = session_snapshot(store.as_ref())
        .await
        .map_err(TestCaseError::fail)?;
    let partial_selection = store
        .claim_ready_queued_work_by_batch_ids(
            SESSION_ID,
            &successor_lease.fence(),
            &successor_owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&first.batch_id),
            crate::testing::queued_work_claim_policy(64),
        )
        .await;
    prop_assert!(
        matches!(
            &partial_selection,
            Err(StoreError::SelectedQueuedWorkRequiresInterruptedComposition {
                required_batch_ids,
            }) if required_batch_ids == &[first.batch_id.clone(), second.batch_id.clone()]
        ),
        "partial selection did not return the literal interrupted composition: {partial_selection:?}"
    );
    assert_snapshot_unchanged(
        store.as_ref(),
        before_partial_selection,
        "partial interrupted-composition selected claim",
    )
    .await
    .map_err(TestCaseError::fail)?;
    let successor_claim = store
        .claim_ready_queued_work_by_batch_ids(
            SESSION_ID,
            &successor_lease.fence(),
            &successor_owner,
            QueuedWorkClaimBoundary::Idle,
            &[first.batch_id.clone(), second.batch_id.clone()],
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("successor did not reclaim full composition"))?;

    let mut state = RuntimeSessionState {
        session_id: SESSION_ID.to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.set_tool_state_snapshot(Some(ToolState::default().with_generation(31)));
    let before = session_snapshot(store.as_ref())
        .await
        .map_err(TestCaseError::fail)?;
    let stale_result = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_queue_claim(stale_claim.completion()),
        )
        .await;
    prop_assert!(
        matches!(
            stale_result,
            Err(StoreError::QueuedWorkClaimSuperseded { .. })
        ),
        "mixed old/new ownership did not reject the whole stale completion: {stale_result:?}"
    );
    assert_snapshot_unchanged(
        store.as_ref(),
        before,
        "reclaim-mediated all-or-nothing rejection",
    )
    .await
    .map_err(TestCaseError::fail)?;
    let pending_while_successor_holds = store
        .list_pending_queued_work(SESSION_ID)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        pending_while_successor_holds.is_empty(),
        "the rejected predecessor commit disturbed successor ownership of the full composition"
    );
    prop_assert!(
        store
            .claim_ready_queued_work_by_batch_ids(
                SESSION_ID,
                &successor_lease.fence(),
                &successor_owner,
                QueuedWorkClaimBoundary::Idle,
                std::slice::from_ref(&first.batch_id),
                crate::testing::queued_work_claim_policy(64),
            )
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?
            .acquired_no_rows(),
        "the rejected predecessor commit released the successor-owned batch"
    );
    store
        .release_session_execution_lease(&successor_lease.completion())
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let pending = store
        .list_pending_queued_work(SESSION_ID)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let pending_ids = pending
        .iter()
        .map(|batch| batch.batch_id.as_str())
        .collect::<BTreeSet<_>>();
    prop_assert_eq!(
        pending_ids,
        BTreeSet::from([first.batch_id.as_str(), second.batch_id.as_str()]),
        "rejected stale completion did not preserve both batches as pending"
    );
    prop_assert_eq!(successor_claim.batches.len(), 2);
    Ok(())
}

async fn law_head_cas_serializes_competing_commits(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let batch = store
        .enqueue_queued_work(queued_draft(0, 0, false))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let input = store
        .enqueue_pending_turn_input(turn_input_draft(0, 0))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let stale_owner = owner(0);
    let stale_lease = store
        .try_claim_session_execution_lease(
            SESSION_ID,
            &stale_owner,
            "head-cas-stale-executor",
            60_000,
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("stale-owner lease busy"))?;
    let stale_work = store
        .claim_ready_queued_work_by_batch_ids(
            SESSION_ID,
            &stale_lease.fence(),
            &stale_owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&batch.batch_id),
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("queued work absent"))?;
    let stale_input = store
        .claim_next_turn_inputs(SESSION_ID, &stale_lease.fence(), &stale_owner, 1)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("turn input absent"))?;
    store
        .release_session_execution_lease(&stale_lease.completion())
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let successor_owner = owner(1);
    let _successor_lease = store
        .try_claim_session_execution_lease(
            SESSION_ID,
            &successor_owner,
            "head-cas-successor-executor",
            60_000,
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("successor lease busy"))?;

    let mut loser_state = RuntimeSessionState {
        session_id: SESSION_ID.to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    loser_state.set_tool_state_snapshot(Some(ToolState::default().with_generation(41)));
    let (loser, _) = RuntimeCommit::persisted_state_for_test(&loser_state, &[])
        .with_operation(crate::OperationId::new(
            crate::ExecutionScope::runtime_operation("runtime-persistence-law:cas-loser"),
            "commit",
        ))
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let loser = loser
        .releasing_session_execution_lease(stale_lease.completion())
        .completing_queue_claim(stale_work.completion())
        .completing_turn_input_claim(stale_input.completion());
    let mut winner_state = RuntimeSessionState {
        session_id: SESSION_ID.to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    winner_state.set_tool_state_snapshot(Some(ToolState::default().with_generation(42)));
    let (winner, _) = RuntimeCommit::persisted_state_for_test(&winner_state, &[])
        .with_operation(crate::OperationId::new(
            crate::ExecutionScope::runtime_operation("runtime-persistence-law:cas-winner"),
            "commit",
        ))
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let winner_result = store
        .commit_runtime_state(winner)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(winner_result.head_revision, 1);
    let before_loser = session_snapshot(store.as_ref())
        .await
        .map_err(TestCaseError::fail)?;
    let loser_result = store.commit_runtime_state(loser).await;
    prop_assert!(
        matches!(loser_result, Err(StoreError::HeadRevisionConflict { .. })),
        "competing CAS loser was not rejected: {loser_result:?}"
    );
    assert_snapshot_unchanged(store.as_ref(), before_loser, "head-CAS loser")
        .await
        .map_err(TestCaseError::fail)?;
    let snapshot = session_snapshot(store.as_ref())
        .await
        .map_err(TestCaseError::fail)?;
    prop_assert_eq!(&snapshot["head"]["head_revision"], &serde_json::json!(1));
    prop_assert_eq!(snapshot["work"].as_array().map(Vec::len), Some(1));
    prop_assert_eq!(snapshot["pending_work"].as_array().map(Vec::len), Some(1));
    prop_assert_eq!(snapshot["pending_inputs"].as_array().map(Vec::len), Some(1));
    prop_assert_eq!(snapshot["applications"].as_array().map(Vec::len), Some(0));
    prop_assert_eq!(&stale_input.inputs[0].input_id, &input.input_id);
    Ok(())
}

async fn law_selected_batch_out_of_order_never_loses_work(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let earlier = store
        .enqueue_queued_work(queued_draft(0, 0, false))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let later = store
        .enqueue_queued_work(queued_draft(1, 1, false))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let owner = owner(0);
    let lease = store
        .try_claim_session_execution_lease(SESSION_ID, &owner, "selected-batch-executor", 60_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("lease busy"))?;
    let claim = store
        .claim_ready_queued_work_by_batch_ids(
            SESSION_ID,
            &lease.fence(),
            &owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&later.batch_id),
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("later batch absent"))?;
    let mut state = RuntimeSessionState {
        session_id: SESSION_ID.to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let result = store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_queue_claim(claim.completion()),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    state.apply_persisted_commit_result(result);
    let remaining = store
        .list_queued_work(SESSION_ID)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(remaining.len(), 1);
    prop_assert_eq!(
        &remaining[0].batch_id,
        &earlier.batch_id,
        "settling batch 2 lost batch 1"
    );
    let claim = store
        .claim_ready_queued_work_by_batch_ids(
            SESSION_ID,
            &lease.fence(),
            &owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&earlier.batch_id),
            crate::testing::queued_work_claim_policy(64),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("earlier batch no longer claimable"))?;
    store
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .completing_queue_claim(claim.completion()),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        store
            .list_queued_work(SESSION_ID)
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?
            .is_empty()
    );
    Ok(())
}

async fn law_turn_inputs_apply_once_in_order(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let first = store
        .enqueue_pending_turn_input(turn_input_draft(0, 0))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let second = store
        .enqueue_pending_turn_input(turn_input_draft(1, 1))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let owner = owner(0);
    let lease = store
        .try_claim_session_execution_lease(SESSION_ID, &owner, "turn-input-executor", 60_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| TestCaseError::fail("lease busy"))?;
    let mut claim = store
        .claim_next_turn_inputs(SESSION_ID, &lease.fence(), &owner, 10)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("turn inputs absent"))?;
    prop_assert_eq!(
        claim
            .inputs
            .iter()
            .map(|input| input.input_id.as_str())
            .collect::<Vec<_>>(),
        vec![first.input_id.as_str(), second.input_id.as_str()]
    );
    claim.record_initial_turn_application(&crate::TurnId::from("ordered-turn"), "ordered-message");
    let expected = claim.applications.clone();
    let state = RuntimeSessionState {
        session_id: SESSION_ID.to_string(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let commit = RuntimeCommit::persisted_state_for_test(&state, &[])
        .completing_turn_input_claim(claim.completion());
    store
        .commit_runtime_state(commit.clone())
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    store
        .commit_runtime_state(commit)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(
        store
            .list_turn_input_applications(SESSION_ID)
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?,
        expected,
        "input applications were reordered or duplicated"
    );
    Ok(())
}

async fn law_commit_atomicity_and_stale_head_non_mutation(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let mut model = ReferenceModel::default();
    let mut shape = RunShape::default();
    for op in [
        RuntimePersistenceOp::ClaimLease { owner: 0 },
        RuntimePersistenceOp::EnqueueWork {
            slot: 0,
            value: 0,
            coalesce: false,
        },
        RuntimePersistenceOp::EnqueueTurnInput { slot: 0, value: 0 },
        RuntimePersistenceOp::ClaimWork {
            selected: false,
            selection: 0,
        },
        RuntimePersistenceOp::ClaimTurnInputs { max_inputs: 2 },
        RuntimePersistenceOp::Commit {
            component_mode: 1,
            value: 7,
            settle_work: true,
            settle_inputs: true,
            stale_head: true,
        },
    ] {
        apply_operation(store.as_ref(), None, &mut model, &mut shape, 11, &op)
            .await
            .map_err(TestCaseError::fail)?;
    }
    prop_assert_eq!(model.head_revision, 0);
    prop_assert_eq!(model.work.len(), 1);
    prop_assert_eq!(model.inputs.len(), 1);
    prop_assert!(model.components.tool_ref.is_none());
    apply_operation(
        store.as_ref(),
        None,
        &mut model,
        &mut shape,
        11,
        &RuntimePersistenceOp::Commit {
            component_mode: 1,
            value: 7,
            settle_work: true,
            settle_inputs: true,
            stale_head: false,
        },
    )
    .await
    .map_err(TestCaseError::fail)?;
    prop_assert_eq!(model.head_revision, 1);
    prop_assert!(model.work.is_empty() && model.inputs.is_empty());
    prop_assert!(
        model.components.tool_ref.is_some()
            && model.components.plugin_ref.is_some()
            && model.components.execution_ref.is_some()
    );
    Ok(())
}

async fn law_checkpoint_refs_track_content(
    store: Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let mut model = ReferenceModel::default();
    let mut shape = RunShape::default();
    for op in [
        RuntimePersistenceOp::Commit {
            component_mode: 1,
            value: 1,
            settle_work: false,
            settle_inputs: false,
            stale_head: false,
        },
        RuntimePersistenceOp::Commit {
            component_mode: 0,
            value: 0,
            settle_work: false,
            settle_inputs: false,
            stale_head: false,
        },
        RuntimePersistenceOp::Commit {
            component_mode: 5,
            value: 0,
            settle_work: false,
            settle_inputs: false,
            stale_head: false,
        },
        RuntimePersistenceOp::Commit {
            component_mode: 1,
            value: 2,
            settle_work: false,
            settle_inputs: false,
            stale_head: false,
        },
        RuntimePersistenceOp::Commit {
            component_mode: 1,
            value: 2,
            settle_work: false,
            settle_inputs: false,
            stale_head: false,
        },
    ] {
        apply_operation(store.as_ref(), None, &mut model, &mut shape, 12, &op)
            .await
            .map_err(TestCaseError::fail)?;
    }
    prop_assert!(shape.checkpoint_stores >= 9);
    prop_assert!(shape.checkpoint_ref_reuses >= 3);
    assert_model_agreement(store.as_ref(), &model)
        .await
        .map_err(TestCaseError::fail)
}
