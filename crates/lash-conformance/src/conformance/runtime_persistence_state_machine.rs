//! Model-based [`RuntimeStore`] laws for drive fences, root admission,
//! queues, inputs, commit CAS, and checkpoint components; process-scoped laws
//! live in the sibling harness.
//!
//! The model is admission honesty (FIG-3927): every row is `Open` or
//! `Admitted{root}`, at most one root is unfinished, a resumed root reads its
//! recorded admission back under any later fence, only that root's fenced
//! commit settles its rows, and its terminal hands the rest back open.

use super::run_shape::Counter;
use super::*;
use crate::store::{AdmittedHead, DriveFence, IngressRowId, IngressSettlement, RootAdmission};
use crate::store::{
    EXECUTION_STATE_CHECKPOINT_COMPONENT, PLUGIN_STATE_CHECKPOINT_COMPONENT,
    TOOL_STATE_CHECKPOINT_COMPONENT,
};
use crate::{
    LeaseOwnerIdentity, PendingTurnInput, PendingTurnInputCancelOutcome, PendingTurnInputDraft,
    PluginNamespaceState, PluginState, QueuedWorkBatch, QueuedWorkBatchDraft, RuntimeCommit,
    RuntimeSessionState, RuntimeStore, RuntimeUsageDeltaIdentity, StoreError, ToolState, TurnId,
    TurnInput, TurnInputIngress, facade_support::ToolStateFacadeOps,
};
use lash_core::testing::RuntimePersistenceTestDriveExt as _;
use lash_core::testing::conformance_support::ToolStateConformanceAccess;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::sync::Arc;
mod attachment_conservation;
mod counterexample;
mod dedicated_laws;
mod generator;
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
    SealFence {
        owner: u8,
    },
    Crash,
    EnqueueWork {
        slot: u8,
        value: u8,
        coalesce: bool,
    },
    AdmitWork,
    AdmitWorkWithStaleFence,
    CancelWork {
        selection: u8,
    },
    EnqueueTurnInput {
        slot: u8,
        value: u8,
    },
    AdmitTurnInputs {
        max_inputs: u8,
    },
    AdmitTurnInputsWithStaleFence,
    CancelAdmittedRow,
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
    SettleUnderStaleFence,
    SettleForeignRow,
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

/// The session's one unfinished root, as its admission recorded it.
#[derive(Clone)]
struct ModeledRoot {
    root: TurnId,
    admission: RootAdmission,
    /// The drive epoch the admission committed under: a read-back under a
    /// later fence is a resume.
    admitted_epoch: u64,
}

impl ModeledRoot {
    fn batch_ids(&self) -> BTreeSet<lash_core::BatchId> {
        self.admission.batch_ids().into_iter().collect()
    }

    fn input_ids(&self) -> BTreeSet<lash_core::InputId> {
        self.admission.input_ids().into_iter().collect()
    }
}

#[derive(Default)]
struct ReferenceModel {
    head_revision: u64,
    has_session: bool,
    current_fence: Option<DriveFence>,
    stale_fences: Vec<DriveFence>,
    work: BTreeMap<String, ModeledWork>,
    inputs: BTreeMap<String, ModeledInput>,
    input_receipts: BTreeMap<String, PendingTurnInputDraft>,
    root: Option<ModeledRoot>,
    root_sequence: u64,
    applications: Vec<crate::TurnInputApplication>,
    components: ComponentModel,
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
    /// The last process-wake sequence a generated enqueue used. It only
    /// rises: a settled wake raises its process's floor, so a later enqueue
    /// is always a fresh wake.
    wake_sequence: u64,
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
    FenceSeals,
    StaleFenceRejections,
    QueueEnqueues,
    QueueAdmissions,
    QueueCompletions,
    AdmissionResumes,
    UnfinishedRootRefusals,
    StaleFenceSettlementRejections,
    RowNotAdmittedRejections,
    CoalescedAdmissions,
    QueueCancellations,
    AdmittedCancelRefusals,
    InputEnqueues,
    InputAdmissions,
    InputApplications,
    InputCancellations,
    RootReleases,
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
}

impl run_shape::Counter for RunShapeCounter {
    const ALL: &'static [Self] = &[
        Self::FenceSeals,
        Self::StaleFenceRejections,
        Self::QueueEnqueues,
        Self::QueueAdmissions,
        Self::QueueCompletions,
        Self::AdmissionResumes,
        Self::UnfinishedRootRefusals,
        Self::StaleFenceSettlementRejections,
        Self::RowNotAdmittedRejections,
        Self::CoalescedAdmissions,
        Self::QueueCancellations,
        Self::AdmittedCancelRefusals,
        Self::InputEnqueues,
        Self::InputAdmissions,
        Self::InputApplications,
        Self::InputCancellations,
        Self::RootReleases,
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
    ];

    fn name(self) -> &'static str {
        match self {
            Self::FenceSeals => "fence_seals",
            Self::StaleFenceRejections => "stale_fence_rejections",
            Self::QueueEnqueues => "queue_enqueues",
            Self::QueueAdmissions => "queue_admissions",
            Self::QueueCompletions => "queue_completions",
            Self::AdmissionResumes => "admission_resumes",
            Self::UnfinishedRootRefusals => "unfinished_root_refusals",
            Self::StaleFenceSettlementRejections => "stale_fence_settlement_rejections",
            Self::RowNotAdmittedRejections => "row_not_admitted_rejections",
            Self::CoalescedAdmissions => "coalesced_admissions",
            Self::QueueCancellations => "queue_cancellations",
            Self::AdmittedCancelRefusals => "admitted_cancel_refusals",
            Self::InputEnqueues => "input_enqueues",
            Self::InputAdmissions => "input_admissions",
            Self::InputApplications => "input_applications",
            Self::InputCancellations => "input_cancellations",
            Self::RootReleases => "root_releases",
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
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

type RunShape = run_shape::RunShape<RunShapeCounter>;
type RunShapeTotals = run_shape::RunShapeTotals<RunShapeCounter>;

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
                assert_required_shape(&shape)?;
                runner_totals.add(&shape);
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

fn assert_required_shape(shape: &RunShape) -> Result<(), TestCaseError> {
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
    store: &dyn RuntimeStore,
    attachment_handles: Option<&RuntimePersistenceStateMachineHandles>,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    seed: u64,
    operation: &RuntimePersistenceOp,
) -> Result<(), String> {
    use RuntimePersistenceOp::*;
    match operation {
        SealFence { owner } => seal_fence(store, model, shape, *owner).await?,
        Crash => crash_after_admission(store, model, shape).await?,
        EnqueueWork {
            slot,
            value,
            coalesce,
        } => {
            // A process wake is the one turn-work payload, and a settled wake
            // raises its process's floor: every generated enqueue is a fresh
            // wake of its slot's process, at the next sequence.
            model.wake_sequence += 1;
            let draft = sequenced_queued_draft(*slot, *value, *coalesce, model.wake_sequence);
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
        AdmitWork => admit_work(store, model, shape).await?,
        AdmitWorkWithStaleFence => {
            admit_with_stale_fence(store, model, shape, Family::Work).await?;
        }
        CancelWork { selection } => {
            let Some(work) = select_open_work(model, *selection) else {
                return Ok(());
            };
            let removed = store
                .cancel_queued_work_batch(&session_id(), &work.batch_id)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "an open batch was not cancellable".to_string())?;
            if removed.batch_id != work.batch_id {
                return Err("queue cancel returned a different batch".to_string());
            }
            model
                .work
                .retain(|_, candidate| candidate.batch.batch_id != removed.batch_id);
            shape[RunShapeCounter::QueueCancellations] += 1;
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
        AdmitTurnInputs { max_inputs } => {
            admit_turn_inputs(store, model, shape, usize::from((*max_inputs).max(1))).await?;
        }
        AdmitTurnInputsWithStaleFence => {
            admit_with_stale_fence(store, model, shape, Family::Inputs).await?;
        }
        CancelAdmittedRow => cancel_admitted_row(store, model, shape).await?,
        CancelTurnInput { selection } => {
            let Some(input) = select_open_input(model, *selection) else {
                return Ok(());
            };
            match store
                .cancel_pending_turn_input(&session_id(), &input.input_id)
                .await
                .map_err(|error| error.to_string())?
            {
                PendingTurnInputCancelOutcome::Cancelled(cancelled) => {
                    model
                        .inputs
                        .retain(|_, candidate| candidate.input.input_id != cancelled.input_id);
                    shape[RunShapeCounter::InputCancellations] += 1;
                }
                other => {
                    return Err(format!(
                        "unexpected cancel outcome for an open modeled input: {other:?}"
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
        SettleUnderStaleFence => settle_under_stale_fence(store, model, shape, seed).await?,
        SettleForeignRow => settle_foreign_row(store, model, shape, seed).await?,
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Family {
    Work,
    Inputs,
}

/// The admission request the model presents for `root` headed by `head`:
/// the fixture's, with the model's own bounds.
fn admission_request(
    fence: &DriveFence,
    root: &TurnId,
    head: AdmittedHead,
    max_inputs: usize,
) -> crate::store::AdmitRootRequest {
    let mut request =
        lash_core::testing::store_fixtures::admit_root_request_for_test(fence, root, head);
    request.max_inputs = max_inputs;
    request.policy = crate::testing::queued_work_claim_policy(4);
    request
}

fn next_root(model: &mut ReferenceModel) -> TurnId {
    model.root_sequence += 1;
    TurnId::from(format!("property-root-{}", model.root_sequence))
}

/// Admit from the head of `family` under the live fence. With a root
/// unfinished, the same root and head read its recorded admission back (a
/// resume), and any other root is refused `UnfinishedRootConflict` without a
/// write; otherwise a new root takes the head's prefix.
async fn admit_from_head(
    store: &dyn RuntimeStore,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    head: AdmittedHead,
    max_inputs: usize,
) -> Result<Option<RootAdmission>, String> {
    let Some(fence) = model.current_fence.clone() else {
        return Ok(None);
    };
    if let Some(unfinished) = model.root.clone() {
        if unfinished.admission.head == head {
            let resumed = store
                .admit_root(&admission_request(
                    &fence,
                    &unfinished.root,
                    head,
                    max_inputs,
                ))
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "a recorded admission did not read back".to_string())?;
            if json(&resumed)? != json(&unfinished.admission)? {
                return Err("a resumed root read back a different admission".to_string());
            }
            if fence.epoch() != unfinished.admitted_epoch {
                shape[RunShapeCounter::AdmissionResumes] += 1;
            }
            return Ok(None);
        }
        let before = session_snapshot(store).await?;
        let other = next_root(model);
        let result = store
            .admit_root(&admission_request(&fence, &other, head, max_inputs))
            .await;
        if !matches!(result, Err(StoreError::UnfinishedRootConflict { .. })) {
            return Err(format!(
                "a second root was admitted while one is unfinished: {result:?}"
            ));
        }
        assert_snapshot_unchanged(store, before, "a second root's admission").await?;
        shape[RunShapeCounter::UnfinishedRootRefusals] += 1;
        return Ok(None);
    }
    let root = next_root(model);
    let admission = store
        .admit_root(&admission_request(&fence, &root, head, max_inputs))
        .await
        .map_err(|error| error.to_string())?;
    if let Some(admission) = &admission {
        model.root = Some(ModeledRoot {
            root,
            admission: admission.clone(),
            admitted_epoch: fence.epoch(),
        });
    }
    Ok(admission)
}

async fn admit_work(
    store: &dyn RuntimeStore,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
) -> Result<(), String> {
    let head = match model.root.as_ref().map(|root| root.admission.head.clone()) {
        Some(head @ AdmittedHead::Batch(_)) => head,
        _ => {
            let Some(first) = open_work(model).into_iter().next() else {
                return Ok(());
            };
            AdmittedHead::Batch(first.batch_id)
        }
    };
    let open = open_work(model);
    let earliest_input = open_inputs(model).first().map(|input| input.enqueue_seq);
    let Some(admission) = admit_from_head(store, model, shape, head.clone(), 64).await? else {
        let AdmittedHead::Batch(head_id) = &head else {
            return Ok(());
        };
        let head_seq = open
            .iter()
            .find(|batch| batch.batch_id == *head_id)
            .map(|batch| batch.enqueue_seq);
        if model.root.is_none()
            && !earliest_input
                .zip(head_seq)
                .is_some_and(|(input, head)| input < head)
        {
            return Err("an open batch head at the front of the lane was not admitted".to_string());
        }
        return Ok(());
    };
    let batches = admission.batch_ids();
    let open_ids = open
        .iter()
        .map(|batch| batch.batch_id.clone())
        .collect::<BTreeSet<_>>();
    if batches
        .first()
        .map(|batch| AdmittedHead::Batch(batch.clone()))
        != Some(head)
        || batches.iter().collect::<BTreeSet<_>>().len() != batches.len()
        || !batches.iter().all(|batch| open_ids.contains(batch))
        || admission.inputs.is_some()
    {
        return Err(format!(
            "a batch-headed admission duplicated, invented or reordered rows: {batches:?}"
        ));
    }
    if let Some(input) = earliest_input
        && open
            .iter()
            .any(|batch| batches.contains(&batch.batch_id) && batch.enqueue_seq > input)
    {
        return Err("a batch-headed admission reached past an earlier open input".to_string());
    }
    if batches.len() > 1 {
        shape[RunShapeCounter::CoalescedAdmissions] += 1;
    }
    shape[RunShapeCounter::QueueAdmissions] += 1;
    Ok(())
}

async fn admit_turn_inputs(
    store: &dyn RuntimeStore,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    max_inputs: usize,
) -> Result<(), String> {
    let head = match model.root.as_ref().map(|root| root.admission.head.clone()) {
        Some(head @ AdmittedHead::Input(_)) => head,
        _ => {
            let Some(first) = open_inputs(model).into_iter().next() else {
                return Ok(());
            };
            AdmittedHead::Input(first.input_id)
        }
    };
    let earliest_batch = open_work(model).first().map(|batch| batch.enqueue_seq);
    let expected = open_inputs(model)
        .into_iter()
        .take_while(|input| earliest_batch.is_none_or(|batch| input.enqueue_seq < batch))
        .take(max_inputs)
        .map(|input| input.input_id)
        .collect::<Vec<_>>();
    let was_unfinished = model.root.is_some();
    let Some(admission) = admit_from_head(store, model, shape, head, max_inputs).await? else {
        if !was_unfinished && !expected.is_empty() {
            return Err("an open input head at the front of the lane was not admitted".to_string());
        }
        return Ok(());
    };
    let actual = admission.input_ids();
    if actual != expected || admission.queued.is_some() {
        return Err(format!(
            "turn inputs were not admitted once in enqueue order: actual={actual:?} expected={expected:?}"
        ));
    }
    shape[RunShapeCounter::InputAdmissions] += 1;
    Ok(())
}

/// A superseded fence admits nothing: the store refuses it before any read.
async fn admit_with_stale_fence(
    store: &dyn RuntimeStore,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    family: Family,
) -> Result<(), String> {
    let Some(stale) = model.stale_fences.last().cloned() else {
        return Ok(());
    };
    let head = match family {
        Family::Work => open_work(model)
            .first()
            .map(|batch| AdmittedHead::Batch(batch.batch_id.clone())),
        Family::Inputs => open_inputs(model)
            .first()
            .map(|input| AdmittedHead::Input(input.input_id.clone())),
    }
    .or_else(|| model.root.as_ref().map(|root| root.admission.head.clone()));
    let Some(head) = head else {
        return Ok(());
    };
    let before = session_snapshot(store).await?;
    let root = next_root(model);
    let result = store
        .admit_root(&admission_request(&stale, &root, head, 64))
        .await;
    if !matches!(result, Err(StoreError::StaleDriveFence { .. })) {
        return Err(format!("a superseded fence admitted rows: {result:?}"));
    }
    assert_snapshot_unchanged(store, before, "superseded-fence admission").await?;
    shape[RunShapeCounter::StaleFenceRejections] += 1;
    Ok(())
}

/// An admitted row is not withdrawable (N5): a host cancel of a row the
/// unfinished root holds answers `AlreadyAdmitted{root}` (a batch: nothing
/// removed) and changes nothing.
async fn cancel_admitted_row(
    store: &dyn RuntimeStore,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
) -> Result<(), String> {
    let Some(root) = model.root.clone() else {
        return Ok(());
    };
    let before = session_snapshot(store).await?;
    if let Some(input) = root.admission.input_ids().first() {
        match store
            .cancel_pending_turn_input(&session_id(), input)
            .await
            .map_err(|error| error.to_string())?
        {
            PendingTurnInputCancelOutcome::AlreadyAdmitted { root: holder, .. }
                if holder == root.root => {}
            other => {
                return Err(format!(
                    "an admitted input was withdrawable or named another root: {other:?}"
                ));
            }
        }
    } else if let Some(batch) = root.admission.batch_ids().first() {
        if store
            .cancel_queued_work_batch(&session_id(), batch)
            .await
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Err("an admitted batch was withdrawable".to_string());
        }
    } else {
        return Ok(());
    }
    assert_snapshot_unchanged(store, before, "a refused cancel of an admitted row").await?;
    shape[RunShapeCounter::AdmittedCancelRefusals] += 1;
    Ok(())
}

async fn seal_fence(
    store: &dyn RuntimeStore,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    owner_index: u8,
) -> Result<(), String> {
    let owner = owner(owner_index);
    let executor = format!("state-machine-executor-{owner_index}");
    let outcome = store
        .seal_drive_epoch_for_test(&session_id(), &owner, &executor, 0)
        .await
        .map_err(|error| error.to_string())?;
    let fence = outcome
        .acquired()
        .ok_or_else(|| "drive epoch seal lost concurrent admission".to_string())?;
    // A later seal supersedes the fence, never the rows its root admitted.
    if let Some(previous) = model.current_fence.replace(fence) {
        model.stale_fences.push(previous);
    }
    shape[RunShapeCounter::FenceSeals] += 1;
    Ok(())
}

/// A worker dies after its admission committed: its fence is superseded and
/// the unfinished root keeps every row it admitted.
async fn crash_after_admission(
    store: &dyn RuntimeStore,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
) -> Result<(), String> {
    let Some(fence) = model.current_fence.take() else {
        return Ok(());
    };
    store
        .supersede_drive_epoch_for_test(&fence)
        .await
        .map_err(|error| error.to_string())?;
    model.stale_fences.push(fence);
    shape[RunShapeCounter::CrashPoints] += 1;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn commit_operation(
    store: &dyn RuntimeStore,
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
    // The unfinished root's final commit, under the live fence: it completes
    // the families the operation names and its terminal hands the rest back.
    let ending = (settle_work || settle_inputs)
        .then(|| model.root.clone().zip(model.current_fence.clone()))
        .flatten();
    let mut settlement = ending
        .as_ref()
        .map(|(root, _)| IngressSettlement::new(root.root.clone()));
    let mut expected_applications = Vec::new();
    if let (Some((root, _)), Some(settlement)) = (ending.as_ref(), settlement.as_mut()) {
        if settle_work && let Some(queued) = &root.admission.queued {
            settlement.completed_batches.push(queued.completion());
        }
        if settle_inputs && let Some(inputs) = &root.admission.inputs {
            let mut inputs = (**inputs).clone();
            inputs.record_initial_turn_application(
                &root.root,
                &format!("property-message-{}", model.operation_sequence),
            );
            expected_applications = inputs.applications.clone();
            settlement.completed_inputs.push(inputs.completion());
        }
    }
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
    if let (Some((root, fence)), Some(settlement)) = (ending.as_ref(), settlement.clone()) {
        commit.drive_fence = Some(Box::new(fence.clone()));
        commit.ingress = Some(settlement);
        commit.root_terminal = Some(Box::new(crate::store::RootTerminalWrite {
            root: root.root.clone(),
            commit: crate::store::TurnCommitId::new(root.root.clone(), 0),
            turn: root.root.clone(),
            stop: None,
        }));
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

    if let (Some((root, _)), Some(settlement)) = (ending, settlement) {
        let completed = settlement.rows().into_iter().collect::<BTreeSet<_>>();
        let completed_batches = root
            .batch_ids()
            .into_iter()
            .filter(|batch| completed.contains(&IngressRowId::Batch(batch.clone())))
            .collect::<BTreeSet<_>>();
        let completed_inputs = root
            .input_ids()
            .into_iter()
            .filter(|input| completed.contains(&IngressRowId::Input(input.clone())))
            .collect::<BTreeSet<_>>();
        model
            .work
            .retain(|_, work| !completed_batches.contains(&work.batch.batch_id));
        model
            .inputs
            .retain(|_, input| !completed_inputs.contains(&input.input.input_id));
        model.applications.extend(expected_applications);
        shape[RunShapeCounter::QueueCompletions] += completed_batches.len() as u64;
        shape[RunShapeCounter::InputApplications] += completed_inputs.len() as u64;
        if root.batch_ids().len() + root.input_ids().len() > completed.len() {
            shape[RunShapeCounter::RootReleases] += 1;
        }
        model.root = None;
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

/// The root's settlement presented under a superseded fence (N4): refused
/// `StaleDriveFence` before anything is written.
async fn settle_under_stale_fence(
    store: &dyn RuntimeStore,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    seed: u64,
) -> Result<(), String> {
    let (Some(root), Some(stale)) = (model.root.clone(), model.stale_fences.last().cloned()) else {
        return Ok(());
    };
    let mut settlement = IngressSettlement::new(root.root.clone());
    if let Some(queued) = &root.admission.queued {
        settlement.completed_batches.push(queued.completion());
    }
    if let Some(inputs) = &root.admission.inputs {
        settlement.completed_inputs.push(inputs.completion());
    }
    let mut commit = fresh_commit(model, seed, "stale-fence")?;
    commit.drive_fence = Some(Box::new(stale));
    commit.ingress = Some(settlement);
    let before = session_snapshot(store).await?;
    let result = store.commit_runtime_state(commit).await;
    if !matches!(result, Err(StoreError::StaleDriveFence { .. })) {
        return Err(format!(
            "a settlement under a superseded fence was not refused: {result:?}"
        ));
    }
    assert_snapshot_unchanged(store, before, "superseded-fence settlement").await?;
    shape[RunShapeCounter::StaleFenceSettlementRejections] += 1;
    Ok(())
}

/// A settlement naming an open row (N10): no root holds it, so the commit is
/// refused `IngressRowNotAdmitted` and writes nothing.
async fn settle_foreign_row(
    store: &dyn RuntimeStore,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    seed: u64,
) -> Result<(), String> {
    let Some(fence) = model.current_fence.clone() else {
        return Ok(());
    };
    let row = open_work(model)
        .first()
        .map(|batch| IngressRowId::Batch(batch.batch_id.clone()))
        .or_else(|| {
            open_inputs(model)
                .first()
                .map(|input| IngressRowId::Input(input.input_id.clone()))
        });
    let Some(row) = row else {
        return Ok(());
    };
    let root = model.root.as_ref().map_or_else(
        || TurnId::from("property-foreign-root"),
        |root| root.root.clone(),
    );
    let mut settlement = IngressSettlement::new(root);
    settlement.released.push(row);
    let mut commit = fresh_commit(model, seed, "foreign-row")?;
    commit.drive_fence = Some(Box::new(fence));
    commit.ingress = Some(settlement);
    let before = session_snapshot(store).await?;
    let result = store.commit_runtime_state(commit).await;
    if !matches!(result, Err(StoreError::IngressRowNotAdmitted { .. })) {
        return Err(format!(
            "a settlement naming an open row was not refused: {result:?}"
        ));
    }
    assert_snapshot_unchanged(store, before, "open-row settlement").await?;
    shape[RunShapeCounter::RowNotAdmittedRejections] += 1;
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
        state.set_execution_state_snapshot(Some(vec![value, value.wrapping_add(1)].into()));
    }
}

fn owner(index: u8) -> LeaseOwnerIdentity {
    LeaseOwnerIdentity::opaque(
        format!("runtime-owner-{index}"),
        format!("incarnation-{index}"),
    )
}

/// Queued work the dedicated laws enqueue outside the generated model: a wake
/// of its own process, so it never meets a generated wake's floor.
fn queued_draft(slot: u8, value: u8, coalesce: bool) -> QueuedWorkBatchDraft {
    wake_draft(
        &format!("runtime-property-fixture-{slot}"),
        value,
        coalesce,
        1,
    )
}

/// The generated model's queued work: a wake of the slot's process at
/// `sequence`.
fn sequenced_queued_draft(
    slot: u8,
    value: u8,
    coalesce: bool,
    sequence: u64,
) -> QueuedWorkBatchDraft {
    wake_draft(
        &format!("runtime-property-work-{slot}"),
        value,
        coalesce,
        sequence,
    )
}

fn wake_draft(process: &str, value: u8, coalesce: bool, sequence: u64) -> QueuedWorkBatchDraft {
    let draft = crate::conformance::helpers::process_wake_work(
        &session_id(),
        process,
        sequence,
        &format!("property-work-{value}"),
        DeliveryPolicy::EarliestSafeBoundary,
    );
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
        TurnInput::text(turn_input_text(value)),
    )
    .with_source_key(format!("runtime-property-input-{slot}"))
}

/// Turn-input text for a generated `value`: a distinct ASCII tag followed by a
/// draw from the adversarial text domain (multi-byte and 4-byte scalars,
/// combining marks, NUL and other controls, lengths around the tool-output
/// truncation budget). A pure function of `value`, so generated cases and the
/// persisted regression corpus replay identically.
fn turn_input_text(value: u8) -> String {
    lash_core::testing::adversarial_text::adversarial_text(
        &format!("runtime property input {value} "),
        u64::from(value),
        lash_core::testing::adversarial_text::TextBudget {
            bytes: 16_000,
            lines: 400,
        },
    )
}

/// The batches no root holds, in `enqueue_seq` order.
fn open_work(model: &ReferenceModel) -> Vec<QueuedWorkBatch> {
    let held = admitted_work_ids(model);
    let mut work = model
        .work
        .values()
        .filter(|work| !held.contains(&work.batch.batch_id))
        .map(|work| work.batch.clone())
        .collect::<Vec<_>>();
    work.sort_by_key(|batch| batch.enqueue_seq);
    work
}

/// The inputs no root holds, in `enqueue_seq` order.
fn open_inputs(model: &ReferenceModel) -> Vec<PendingTurnInput> {
    let held = admitted_input_ids(model);
    let mut inputs = model
        .inputs
        .values()
        .filter(|input| !held.contains(&input.input.input_id))
        .map(|input| input.input.clone())
        .collect::<Vec<_>>();
    inputs.sort_by_key(|input| input.enqueue_seq);
    inputs
}

fn admitted_work_ids(model: &ReferenceModel) -> BTreeSet<lash_core::BatchId> {
    model
        .root
        .as_ref()
        .map(ModeledRoot::batch_ids)
        .unwrap_or_default()
}

fn admitted_input_ids(model: &ReferenceModel) -> BTreeSet<lash_core::InputId> {
    model
        .root
        .as_ref()
        .map(ModeledRoot::input_ids)
        .unwrap_or_default()
}

fn select_open_work(model: &ReferenceModel, selection: u8) -> Option<QueuedWorkBatch> {
    let values = open_work(model);
    values
        .get(usize::from(selection) % values.len().max(1))
        .cloned()
}

fn select_open_input(model: &ReferenceModel, selection: u8) -> Option<PendingTurnInput> {
    let values = open_inputs(model);
    values
        .get(usize::from(selection) % values.len().max(1))
        .cloned()
}

/// Enforce queue-depth conservation through stronger element-wise agreement on
/// both queue read seams. Exact equality for total and pending queued work
/// subsumes a separate cardinality law, while the model's unfinished root
/// defines the admitted remainder.
async fn assert_model_agreement(
    store: &dyn RuntimeStore,
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
        .list_open_queued_work(&session_id())
        .await
        .map_err(|error| error.to_string())?;
    if json(&actual_pending)? != json(&open_work(model))? {
        return Err("open queued-work projection differs from the admission model".to_string());
    }

    let actual_inputs = store
        .list_pending_turn_inputs(&session_id())
        .await
        .map_err(|error| error.to_string())?;
    if json(&actual_inputs)? != json(&pending_input_read_model::pending_input_reads(model))? {
        return Err(
            "pending turn-input projection differs from lifecycle and admission model".to_string(),
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

async fn session_snapshot(store: &dyn RuntimeStore) -> Result<serde_json::Value, String> {
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
        "pending_work": store.list_open_queued_work(&session_id()).await.map_err(|error| error.to_string())?,
        "pending_inputs": store.list_pending_turn_inputs(&session_id()).await.map_err(|error| error.to_string())?,
        "applications": store.list_turn_input_applications(&session_id()).await.map_err(|error| error.to_string())?,
    }))
}

async fn assert_snapshot_unchanged(
    store: &dyn RuntimeStore,
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
