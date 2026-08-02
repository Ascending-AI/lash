//! Model-based property laws for the durable process and runtime stores.
//!
//! The generator lives here so every backend replays exactly the same operation
//! language through the public trait-object contracts. Backend crates only
//! provide fresh handles; they do not carry a `proptest` dependency.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, RngSeed, TestError, TestRunner};

use super::*;
use crate::{
    LeaseOwnerIdentity, ProcessCompletionOutcome, ProcessExecutionWriteAuthority,
    ProcessExternalRef, ProcessLease, ProcessLeaseClaimOutcome, ProcessObserverBy, ProcessRecord,
    ProcessStartOutcome, ProjectionWatermark, WakeDelivery, WakeDeliveryClaimOutcome,
    WakeDeliveryState, WakeDiscardReason, apply_process_event_projection, fold_process_record,
    process_wake_batch_draft,
};

const PROCESS_COUNT: u8 = 3;
const SESSION_COUNT: u8 = 2;
const DEFAULT_CASES: u32 = 32;
const DEFAULT_RUNNER_SEED: u64 = 830;
const MAX_OPS: usize = 48;
const GENERATED_PREFIX_OPS: usize = 8;
const DEDICATED_LAW_SEED: u64 = 0xded1_ca7e;

/// Fresh process-registry and runtime-persistence handles for one generated case.
pub struct StoreContractHandles {
    pub registry: Arc<dyn ProcessRegistry>,
    pub runtime: Arc<dyn RuntimePersistence>,
}

/// The generated operation alphabet shared by every durable store backend.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum StoreContractOp {
    Register {
        process: u8,
        disposition: u8,
        max_attempts: u8,
        wake_target: Option<u8>,
    },
    FirstStart {
        process: u8,
        owner: u8,
        attempt: u8,
    },
    EnterWait {
        process: u8,
        stale: bool,
    },
    ClearWait {
        process: u8,
        stale: bool,
    },
    SetExternalRef {
        process: u8,
        value: u8,
    },
    Signal {
        process: u8,
        replay: u8,
        value: u8,
        wake: bool,
        stale: bool,
    },
    CancelRequest {
        process: u8,
        reason: u8,
    },
    Terminal {
        process: u8,
        disposition: u8,
    },
    AddObserver {
        process: u8,
        session: u8,
    },
    RemoveObserver {
        process: u8,
        session: u8,
    },
    Retarget {
        process: u8,
        session: Option<u8>,
    },
    ClaimLease {
        process: u8,
        owner: u8,
    },
    ReleaseLease {
        process: u8,
        stale: bool,
    },
    ClaimWake,
    MarkWake {
        stale: bool,
    },
    DiscardWake {
        stale: bool,
    },
    DeferWake {
        stale: bool,
    },
    EnqueueWake {
        process: u8,
    },
    ConsumeWake {
        selection: u8,
        highest_in_group: bool,
        stale: bool,
    },
    Prune {
        watermark: bool,
    },
    CompactTombstones {
        caught_up: bool,
    },
}

/// Stateful driver for the shared generated store-contract operation language.
///
/// This deliberately performs only the operation semantics and the small
/// amount of bookkeeping needed by later operations (current authorities,
/// leases, wake claims, and queue selections). The property harness layers its
/// reference-model laws on top; cross-backend differential tests use the same
/// driver but provide their own backend-agreement oracle.
pub struct StoreContractScenario {
    handles: StoreContractHandles,
    model: ReferenceModel,
    shape: RunShape,
}

impl StoreContractScenario {
    pub fn new(handles: StoreContractHandles) -> Self {
        Self {
            handles,
            model: ReferenceModel::default(),
            shape: RunShape::default(),
        }
    }

    pub async fn apply(&mut self, operation: &StoreContractOp) -> Result<(), String> {
        apply_operation(&self.handles, &mut self.model, &mut self.shape, operation).await
    }
}

#[derive(Clone, Debug, serde::Deserialize)]
struct GeneratedCase {
    seed: u64,
    operations: Vec<StoreContractOp>,
}

#[derive(Clone, Debug, Default)]
struct ModelProcess {
    base: Option<ProcessRecord>,
    expected_record: Option<ProcessRecord>,
    observers: BTreeSet<String>,
    current_authority: Option<ProcessExecutionWriteAuthority>,
    superseded_authorities: Vec<ProcessExecutionWriteAuthority>,
    leases: Vec<ProcessLease>,
    tombstoned: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct ExpectedQueuedWake {
    wake: crate::ProcessWakeDelivery,
    delivery_policy: DeliveryPolicy,
    slot_policy: SlotPolicy,
    merge_key: MergeKey,
    available_at_ms: u64,
}

#[derive(Clone, Debug, Default)]
struct ReferenceModel {
    processes: BTreeMap<String, ModelProcess>,
    wake_deliveries: BTreeMap<String, WakeDelivery>,
    live_wakes: BTreeMap<(String, String), BTreeMap<u64, ExpectedQueuedWake>>,
    next_wake_sequence: BTreeMap<(String, String), u64>,
    projection_cursor: ProcessChangeCursor,
}

#[derive(Clone, Copy, Debug, Default)]
struct RunShape {
    enqueues_committed: u64,
    consumes_committed: u64,
    out_of_order_states: u64,
}

#[derive(Debug, Default)]
struct RunShapeTotals {
    enqueues_committed: AtomicU64,
    consumes_committed: AtomicU64,
    out_of_order_states: AtomicU64,
}

impl RunShapeTotals {
    fn add(&self, shape: RunShape) {
        self.enqueues_committed
            .fetch_add(shape.enqueues_committed, Ordering::Relaxed);
        self.consumes_committed
            .fetch_add(shape.consumes_committed, Ordering::Relaxed);
        self.out_of_order_states
            .fetch_add(shape.out_of_order_states, Ordering::Relaxed);
    }
}

impl ReferenceModel {
    fn process_mut(&mut self, id: &str) -> &mut ModelProcess {
        self.processes.entry(id.to_string()).or_default()
    }
}

impl ModelProcess {
    fn reset_to_tombstone(&mut self) {
        *self = Self {
            tombstoned: true,
            ..Self::default()
        };
    }

    fn install_fresh(&mut self, record: ProcessRecord) {
        *self = Self {
            base: Some(record.clone()),
            expected_record: Some(record),
            ..Self::default()
        };
    }
}

/// Run the named store-contract laws with proptest shrinking.
///
/// On failure this writes the case seed and the minimized operation trace before
/// panicking, so a backend defect remains reproducible even when the test log is
/// unavailable.
pub async fn store_contract_state_machine<F, Fut>(backend: &'static str, make: F)
where
    F: Fn(u64) -> Fut + Send + Sync + Clone + 'static,
    Fut: Future<Output = StoreContractHandles> + Send + 'static,
{
    let cases = std::env::var("LASH_STORE_CONTRACT_PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_CASES);
    let runner_seed = std::env::var("LASH_STORE_CONTRACT_PROPTEST_SEED")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_RUNNER_SEED);
    let config = Config {
        cases,
        max_shrink_iters: 8_192,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(runner_seed),
        ..Config::default()
    };
    if let Err(error) = assert_dedicated_laws(&make, DEDICATED_LAW_SEED).await {
        panic!("{backend} dedicated store-contract law failed: {error}");
    }
    if let Err(error) = replay_regression_corpus(&make).await {
        panic!("{backend} store-contract regression corpus failed: {error}");
    }

    let runtime = tokio::runtime::Handle::current();
    let shape_totals = Arc::new(RunShapeTotals::default());
    let runner_shape_totals = Arc::clone(&shape_totals);
    let result = tokio::task::spawn_blocking(move || {
        let mut runner = TestRunner::new(config);
        runner.run(&generated_case(), |case| {
            runtime.block_on(async {
                let handles = make(case.seed).await;
                let shape = replay_case(handles, &case.operations).await?;
                prop_assert!(
                    shape.consumes_committed > 0,
                    "generated alphabet starvation: case committed no wake consumes"
                );
                prop_assert!(
                    shape.out_of_order_states > 0,
                    "generated alphabet starvation: case reached no out-of-order settlement state"
                );
                runner_shape_totals.add(shape);
                Ok(())
            })
        })
    })
    .await
    .expect("store-contract property runner task");

    if let Err(error) = result {
        persist_counterexample(backend, runner_seed, &error);
        panic!(
            "{backend} store-contract property law failed with runner seed {runner_seed}; replay with LASH_STORE_CONTRACT_PROPTEST_SEED={runner_seed}: {error}"
        );
    }
    eprintln!(
        "store-contract run shape ({backend}, cases={cases}): enqueues_committed={} consumes_committed={} out_of_order_states={}",
        shape_totals.enqueues_committed.load(Ordering::Relaxed),
        shape_totals.consumes_committed.load(Ordering::Relaxed),
        shape_totals.out_of_order_states.load(Ordering::Relaxed),
    );
}

fn generated_case() -> impl Strategy<Value = GeneratedCase> {
    (
        any::<u64>(),
        prop::collection::vec(operation(), 1..=(MAX_OPS - GENERATED_PREFIX_OPS)),
    )
        .prop_map(|(seed, random_operations)| {
            let mut operations = generated_prefix();
            operations.extend(random_operations);
            GeneratedCase { seed, operations }
        })
}

/// Deterministically sample the shared store-contract operation alphabet.
///
/// The required prefix prevents small differential budgets from starving the
/// process, lease, wake-delivery, queue, and prune surfaces. Remaining steps
/// come from the exact strategy used by the property-law harness.
pub fn sample_store_contract_operations(runner_seed: u64, max_ops: usize) -> Vec<StoreContractOp> {
    let mut operations = generated_prefix();
    operations.truncate(max_ops);
    if operations.len() == max_ops {
        return operations;
    }

    let mut runner = TestRunner::new(Config {
        cases: 1,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(runner_seed),
        ..Config::default()
    });
    while operations.len() < max_ops {
        operations.push(
            operation()
                .new_tree(&mut runner)
                .expect("store-contract operation strategy must generate")
                .current(),
        );
    }
    operations
}

fn generated_prefix() -> Vec<StoreContractOp> {
    vec![
        StoreContractOp::Register {
            process: 0,
            disposition: 0,
            max_attempts: 3,
            wake_target: Some(0),
        },
        StoreContractOp::FirstStart {
            process: 0,
            owner: 0,
            attempt: 1,
        },
        StoreContractOp::FirstStart {
            process: 0,
            owner: 1,
            attempt: 2,
        },
        StoreContractOp::EnterWait {
            process: 0,
            stale: true,
        },
        StoreContractOp::EnqueueWake { process: 0 },
        StoreContractOp::EnqueueWake { process: 0 },
        StoreContractOp::ConsumeWake {
            selection: 0,
            highest_in_group: true,
            stale: false,
        },
        StoreContractOp::Prune { watermark: false },
    ]
}

fn operation() -> impl Strategy<Value = StoreContractOp> {
    prop_oneof![
        4 => (0..PROCESS_COUNT, 0_u8..3, 1_u8..4, prop::option::of(0..SESSION_COUNT))
            .prop_map(|(process, disposition, max_attempts, wake_target)| StoreContractOp::Register {
                process, disposition, max_attempts, wake_target,
            }),
        4 => (0..PROCESS_COUNT, 0_u8..3, 1_u8..5)
            .prop_map(|(process, owner, attempt)| StoreContractOp::FirstStart { process, owner, attempt }),
        2 => (0..PROCESS_COUNT, any::<bool>())
            .prop_map(|(process, stale)| StoreContractOp::EnterWait { process, stale }),
        2 => (0..PROCESS_COUNT, any::<bool>())
            .prop_map(|(process, stale)| StoreContractOp::ClearWait { process, stale }),
        2 => (0..PROCESS_COUNT, 0_u8..3)
            .prop_map(|(process, value)| StoreContractOp::SetExternalRef { process, value }),
        5 => (0..PROCESS_COUNT, 0_u8..4, any::<u8>(), any::<bool>(), any::<bool>())
            .prop_map(|(process, replay, value, wake, stale)| StoreContractOp::Signal { process, replay, value, wake, stale }),
        2 => (0..PROCESS_COUNT, any::<u8>())
            .prop_map(|(process, reason)| StoreContractOp::CancelRequest { process, reason }),
        2 => (0..PROCESS_COUNT, 0_u8..4)
            .prop_map(|(process, disposition)| StoreContractOp::Terminal { process, disposition }),
        2 => (0..PROCESS_COUNT, 0..SESSION_COUNT)
            .prop_map(|(process, session)| StoreContractOp::AddObserver { process, session }),
        2 => (0..PROCESS_COUNT, 0..SESSION_COUNT)
            .prop_map(|(process, session)| StoreContractOp::RemoveObserver { process, session }),
        2 => (0..PROCESS_COUNT, prop::option::of(0..SESSION_COUNT))
            .prop_map(|(process, session)| StoreContractOp::Retarget { process, session }),
        2 => (0..PROCESS_COUNT, 0_u8..3)
            .prop_map(|(process, owner)| StoreContractOp::ClaimLease { process, owner }),
        2 => (0..PROCESS_COUNT, any::<bool>())
            .prop_map(|(process, stale)| StoreContractOp::ReleaseLease { process, stale }),
        2 => Just(StoreContractOp::ClaimWake),
        2 => any::<bool>().prop_map(|stale| StoreContractOp::MarkWake { stale }),
        2 => any::<bool>().prop_map(|stale| StoreContractOp::DiscardWake { stale }),
        2 => any::<bool>().prop_map(|stale| StoreContractOp::DeferWake { stale }),
        3 => (0..PROCESS_COUNT)
            .prop_map(|process| StoreContractOp::EnqueueWake { process }),
        3 => (any::<u8>(), any::<bool>(), any::<bool>())
            .prop_map(|(selection, highest_in_group, stale)| StoreContractOp::ConsumeWake { selection, highest_in_group, stale }),
        2 => any::<bool>().prop_map(|watermark| StoreContractOp::Prune { watermark }),
        2 => any::<bool>().prop_map(|caught_up| StoreContractOp::CompactTombstones { caught_up }),
    ]
}

async fn replay_case(
    handles: StoreContractHandles,
    operations: &[StoreContractOp],
) -> Result<RunShape, TestCaseError> {
    let mut scenario = StoreContractScenario::new(handles);
    for (step, operation) in operations.iter().enumerate() {
        scenario.apply(operation).await.map_err(|reason| {
            TestCaseError::fail(format!("step {step} {operation:?}: {reason}"))
        })?;
        assert_fold_law(&scenario.handles.registry, &scenario.model)
            .await
            .map_err(|reason| TestCaseError::fail(format!("Fold at step {step}: {reason}")))?;
        assert_model_agreement(&scenario.handles, &scenario.model)
            .await
            .map_err(|reason| {
                TestCaseError::fail(format!("model agreement at step {step}: {reason}"))
            })?;
    }
    Ok(scenario.shape)
}

async fn replay_regression_corpus<F, Fut>(make: &F) -> Result<(), TestCaseError>
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = StoreContractHandles>,
{
    let cases: Vec<GeneratedCase> =
        serde_json::from_str(include_str!("store_contract_regressions.json"))
            .map_err(|error| TestCaseError::fail(format!("invalid regression corpus: {error}")))?;
    for (index, case) in cases.iter().enumerate() {
        let handles = make(case.seed).await;
        replay_case(handles, &case.operations)
            .await
            .map_err(|reason| TestCaseError::fail(format!("regression case {index}: {reason}")))?;
    }
    Ok(())
}

async fn assert_dedicated_laws<F, Fut>(make: &F, seed: u64) -> Result<(), TestCaseError>
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = StoreContractHandles>,
{
    assert_on_fresh_handles(make, seed, |handles| async move {
        assert_replay_key_idempotency(&handles.registry).await
    })
    .await?;
    assert_on_fresh_handles(make, seed, |handles| async move {
        assert_attempt_monotonicity_and_budget(&handles.registry).await
    })
    .await?;
    assert_on_fresh_handles(make, seed, |handles| async move {
        assert_stale_authority_non_mutation(&handles.registry).await
    })
    .await?;
    assert_on_fresh_handles(make, seed, |handles| async move {
        assert_wake_group_order_and_claim_ownership(&handles.registry).await
    })
    .await?;
    assert_on_fresh_handles(make, seed, |handles| async move {
        assert_enqueued_wake_high_water_safety(&handles.runtime).await
    })
    .await?;
    assert_on_fresh_handles(make, seed, |handles| async move {
        assert_prune_tombstone_watermark_safety(&handles.registry).await
    })
    .await?;
    assert_on_fresh_handles(make, seed, |handles| async move {
        assert_prune_reregister_registry_state_is_fresh(&handles.registry).await
    })
    .await
}

async fn assert_on_fresh_handles<F, Fut, Law, LawFut>(
    make: &F,
    seed: u64,
    law: Law,
) -> Result<(), TestCaseError>
where
    F: Fn(u64) -> Fut,
    Fut: Future<Output = StoreContractHandles>,
    Law: FnOnce(StoreContractHandles) -> LawFut,
    LawFut: Future<Output = Result<(), TestCaseError>>,
{
    // Dedicated laws always construct handles here; generated-run handles never enter this path.
    law(make(seed).await).await
}

fn process_id(index: u8) -> String {
    format!("prop-process-{}", index % PROCESS_COUNT)
}

fn session_id(index: u8) -> String {
    format!("prop-session-{}", index % SESSION_COUNT)
}

fn disposition(index: u8) -> RecoveryDisposition {
    match index % 3 {
        0 => RecoveryDisposition::Rerunnable,
        1 => RecoveryDisposition::OwnerBound,
        _ => RecoveryDisposition::ExternallyOwned,
    }
}

fn registration(
    process_id: &str,
    disposition: RecoveryDisposition,
    max_attempts: u32,
    wake_target: Option<String>,
) -> ProcessRegistration {
    ProcessRegistration::new(
        process_id,
        ProcessInput::Engine {
            kind: "store-contract-property".to_string(),
            payload: serde_json::Value::Null,
        },
        disposition,
        ProcessProvenance::host(),
    )
    .with_max_attempts(Some(max_attempts))
    .with_execution_env_ref(Some(ProcessExecutionEnvRef::new(format!(
        "process-env:{process_id}"
    ))))
    .with_extra_event_types([
        ProcessEventType {
            name: "property.signal".to_string(),
            payload_schema: LashSchema::any(),
            semantics: ProcessEventSemanticsSpec::default(),
        },
        ProcessEventType {
            name: "property.wake".to_string(),
            payload_schema: LashSchema::any(),
            semantics: ProcessEventSemanticsSpec {
                wake: Some(ProcessWakeSpec {
                    when: Some(ProcessValueSelector::Present("/wake_input".to_string())),
                    input: ProcessValueSelector::Pointer("/wake_input".to_string()),
                }),
                ..ProcessEventSemanticsSpec::default()
            },
        },
    ])
    .with_wake_session_id(wake_target)
}

fn invocation_authority(
    process_id: &str,
    owner: u8,
    attempt: u32,
) -> ProcessExecutionWriteAuthority {
    ProcessExecutionWriteAuthority::invocation(
        process_id,
        format!("owner-{owner}-attempt-{attempt}"),
    )
    .bind_attempt(attempt)
}

fn stale_authority(process_id: &str) -> ProcessExecutionWriteAuthority {
    invocation_authority(process_id, 250, 250)
}

fn wait_state(process_id: &str) -> WaitState {
    WaitState {
        since_ms: 1,
        kind: WaitKind::Signal {
            name: "property".to_string(),
            event_type: "property.signal".to_string(),
            key: format!("{process_id}:wait"),
            ordinal: 1,
        },
    }
}

async fn apply_operation(
    handles: &StoreContractHandles,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    operation: &StoreContractOp,
) -> Result<(), String> {
    match operation {
        StoreContractOp::Register {
            process,
            disposition: d,
            max_attempts,
            wake_target,
        } => {
            let id = process_id(*process);
            let target = wake_target.map(session_id);
            let result = handles
                .registry
                .register_process(registration(
                    &id,
                    disposition(*d),
                    u32::from(*max_attempts),
                    target.clone(),
                ))
                .await;
            if let Ok(record) = result {
                let entry = model.process_mut(&id);
                // The store permits registry-row reuse after prune, although hosts must not
                // reuse ids across prune horizons because receiver wake high-water survives.
                // Keep this generated registry lifecycle to pin the narrower store behavior.
                if entry.base.is_none() || entry.tombstoned {
                    entry.install_fresh(record);
                } else {
                    entry.base.get_or_insert(record);
                }
            }
        }
        StoreContractOp::FirstStart {
            process,
            owner,
            attempt,
        } => {
            let id = process_id(*process);
            let authority = invocation_authority(&id, *owner, u32::from(*attempt));
            let Some(started) = authority.invocation_started() else {
                unreachable!()
            };
            let outcome = handles
                .registry
                .record_first_started_with_authority(&id, started.clone(), &authority)
                .await
                .map_err(|error| error.to_string());
            if let Ok(ProcessStartOutcome::Started(_)) = outcome {
                let entry = model.process_mut(&id);
                if let Some(previous) = entry.current_authority.replace(authority) {
                    entry.superseded_authorities.push(previous);
                }
                if let Some(expected) = entry.expected_record.as_mut() {
                    expected.first_started = Some(Box::new(started));
                }
            }
        }
        StoreContractOp::EnterWait { process, stale } => {
            let id = process_id(*process);
            let authority = selected_authority(model, &id, *stale);
            let must_reject = *stale && has_current_authority(model, &id);
            let before = registry_snapshot(&handles.registry, &id).await;
            let result = handles
                .registry
                .set_process_wait_with_authority(&id, wait_state(&id), &authority)
                .await;
            assert_typed_stale_authority_rejection(&result, must_reject, &id, "enter wait")?;
            assert_rejected_write_is_noop(
                &handles.registry,
                &id,
                before,
                result.is_err(),
                "Stale-authority non-mutation",
            )
            .await?;
            if result.is_ok()
                && let Some(expected) = model.process_mut(&id).expected_record.as_mut()
            {
                expected.wait = Some(wait_state(&id));
                expected.status = crate::ProcessStatus::Waiting;
            }
        }
        StoreContractOp::ClearWait { process, stale } => {
            let id = process_id(*process);
            let authority = selected_authority(model, &id, *stale);
            let must_reject = *stale && has_current_authority(model, &id);
            let before = registry_snapshot(&handles.registry, &id).await;
            let result = handles
                .registry
                .clear_process_wait_with_authority(&id, &authority)
                .await;
            assert_typed_stale_authority_rejection(&result, must_reject, &id, "clear wait")?;
            assert_rejected_write_is_noop(
                &handles.registry,
                &id,
                before,
                result.is_err(),
                "Stale-authority non-mutation",
            )
            .await?;
            if result.is_ok()
                && let Some(expected) = model.process_mut(&id).expected_record.as_mut()
            {
                expected.wait = None;
                if !expected.is_terminal() {
                    expected.status = crate::ProcessStatus::Running;
                }
            }
        }
        StoreContractOp::SetExternalRef { process, value } => {
            let id = process_id(*process);
            let external_ref = ProcessExternalRef {
                backend: "property".to_string(),
                id: format!("external-{value}"),
                metadata: None,
            };
            if handles
                .registry
                .set_external_ref(&id, external_ref.clone())
                .await
                .is_ok()
                && let Some(expected) = model.process_mut(&id).expected_record.as_mut()
                && expected.external_ref.is_none()
            {
                expected.external_ref = Some(external_ref);
            }
        }
        StoreContractOp::Signal {
            process,
            replay,
            value,
            wake,
            stale,
        } => {
            let id = process_id(*process);
            let (event_type, payload) = if *wake {
                ("property.wake", serde_json::json!({"wake_input": value}))
            } else {
                ("property.signal", serde_json::json!({"value": value}))
            };
            let request = ProcessEventAppendRequest::new(event_type, payload)
                .with_replay_key(format!("{id}:property:{replay}"));
            let before = registry_snapshot(&handles.registry, &id).await;
            let must_reject = *stale && has_current_authority(model, &id);
            let result = if *stale {
                let authority = selected_authority(model, &id, true);
                handles
                    .registry
                    .append_event_with_authority(&id, request, &authority)
                    .await
            } else if let Some(authority) = model
                .processes
                .get(&id)
                .and_then(|process| process.current_authority.as_ref())
            {
                handles
                    .registry
                    .append_event_with_authority(&id, request, authority)
                    .await
            } else {
                handles.registry.append_event(&id, request).await
            };
            assert_typed_stale_authority_rejection(&result, must_reject, &id, "append event")?;
            assert_rejected_write_is_noop(
                &handles.registry,
                &id,
                before,
                result.is_err(),
                "Replay-key idempotency / stale append",
            )
            .await?;
            if let Ok(appended) = result {
                if let Some(expected) = model.process_mut(&id).expected_record.as_mut() {
                    apply_process_event_projection(expected, &appended.event)
                        .map_err(|error| error.to_string())?;
                }
                if let Some(wake) = appended.wake_delivery {
                    let delivery =
                        WakeDelivery::pending(wake, handles.registry.wake_delivery_config())
                            .map_err(|error| error.to_string())?;
                    model
                        .wake_deliveries
                        .entry(delivery.delivery_id.clone())
                        .or_insert(delivery);
                }
            }
        }
        StoreContractOp::CancelRequest { process, reason } => {
            let id = process_id(*process);
            if let Ok(appended) = handles
                .registry
                .append_event(
                    &id,
                    ProcessEventAppendRequest::cancel_requested(
                        &id,
                        Some(format!("reason-{reason}")),
                    ),
                )
                .await
                && let Some(expected) = model.process_mut(&id).expected_record.as_mut()
            {
                apply_process_event_projection(expected, &appended.event)
                    .map_err(|error| error.to_string())?;
            }
        }
        StoreContractOp::Terminal {
            process,
            disposition: terminal,
        } => {
            let id = process_id(*process);
            let output = terminal_output(*terminal);
            if let Ok(Some(record)) = handles.registry.get_process(&id).await {
                let authority = match record.disposition {
                    RecoveryDisposition::ExternallyOwned => {
                        ProcessCompletionAuthority::external_owner()
                    }
                    _ => ProcessCompletionAuthority::workflow_key(format!("property:{id}")),
                };
                if let Ok(ProcessCompletionOutcome::Committed(_)) = handles
                    .registry
                    .complete_process(&id, output.clone(), authority)
                    .await
                    && let Some(expected) = model.process_mut(&id).expected_record.as_mut()
                {
                    expected.wait = None;
                    expected.status = output
                        .terminal_status()
                        .expect("generated output is terminal");
                    expected.outcome = Some(output);
                }
            }
        }
        StoreContractOp::AddObserver { process, session } => {
            let id = process_id(*process);
            let session = session_id(*session);
            if handles
                .registry
                .add_observer(&session, &id, ProcessObserverBy::host("property"))
                .await
                .is_ok()
            {
                model.process_mut(&id).observers.insert(session);
            }
        }
        StoreContractOp::RemoveObserver { process, session } => {
            let id = process_id(*process);
            let session = session_id(*session);
            if handles
                .registry
                .remove_observer(&session, &id, ProcessObserverBy::host("property"))
                .await
                .is_ok()
            {
                model.process_mut(&id).observers.remove(&session);
            }
        }
        StoreContractOp::Retarget { process, session } => {
            let id = process_id(*process);
            let target = session.map(session_id);
            if handles
                .registry
                .retarget_subscription(&id, target.as_deref())
                .await
                .is_ok()
            {
                for delivery in model.wake_deliveries.values_mut() {
                    if delivery.state == WakeDeliveryState::Pending
                        && delivery.wake.process_id == id
                        && Some(delivery.wake.target_session_id.as_str()) != target.as_deref()
                    {
                        delivery.state = WakeDeliveryState::Discarded;
                        delivery.discard_reason = Some(WakeDiscardReason::Retargeted);
                    }
                }
            }
        }
        StoreContractOp::ClaimLease { process, owner } => {
            let id = process_id(*process);
            if let Ok(ProcessLeaseClaimOutcome::Acquired(lease)) = handles
                .registry
                .claim_process_lease(
                    &id,
                    &LeaseOwnerIdentity::opaque(
                        format!("owner-{owner}"),
                        format!("incarnation-{owner}"),
                    ),
                    60_000,
                )
                .await
            {
                let leases = &mut model.process_mut(&id).leases;
                if let Some(current) = leases
                    .last_mut()
                    .filter(|current| current.lease_token == lease.lease_token)
                {
                    // Same-incarnation re-entry renews the current authority;
                    // it does not create an older authority that can be used
                    // as a stale completion.
                    *current = lease;
                } else {
                    leases.push(lease);
                }
            }
        }
        StoreContractOp::ReleaseLease { process, stale } => {
            let id = process_id(*process);
            let Some(leases) = model.processes.get(&id).map(|process| &process.leases) else {
                return Ok(());
            };
            if let Some(lease) = if *stale && leases.len() > 1 {
                leases.first()
            } else {
                leases.last()
            } {
                let before = process_lease_snapshot(&handles.registry, &id).await?;
                let completion = crate::ProcessLeaseCompletion::from_lease(lease);
                handles
                    .registry
                    .complete_process_lease(&completion)
                    .await
                    .map_err(|error| error.to_string())?;
                let after = process_lease_snapshot(&handles.registry, &id).await?;
                if *stale && leases.len() > 1 {
                    if before != after {
                        return Err(
                            "Stale-authority non-mutation: stale lease release changed the live lease"
                                .to_string(),
                        );
                    }
                } else if after.is_some() {
                    return Err("current lease release left a live lease behind".to_string());
                }
            }
        }
        StoreContractOp::ClaimWake => {
            let claims = handles
                .registry
                .claim_pending_wake_deliveries(3)
                .await
                .map_err(|error| error.to_string())?;
            for claim in claims {
                model
                    .wake_deliveries
                    .insert(claim.delivery_id.clone(), claim);
            }
        }
        StoreContractOp::MarkWake { stale } => {
            settle_wake(handles, model, *stale, WakeSettle::Mark).await?
        }
        StoreContractOp::DiscardWake { stale } => {
            settle_wake(handles, model, *stale, WakeSettle::Discard).await?
        }
        StoreContractOp::DeferWake { stale } => {
            settle_wake(handles, model, *stale, WakeSettle::Defer).await?
        }
        StoreContractOp::EnqueueWake { process } => {
            let process = process_id(*process);
            let key = ("prop-runtime-session".to_string(), process.clone());
            let sequence = model.next_wake_sequence.entry(key.clone()).or_insert(1);
            let wake = runtime_wake(&process, *sequence);
            let draft = process_wake_batch_draft(wake.clone());
            let receipt = handles
                .runtime
                .enqueue_queued_work(draft.clone())
                .await
                .map_err(|error| error.to_string())?;
            if receipt.enqueue_seq == 0 {
                return Err(format!(
                    "Enqueued-wake high-water safety: fresh contiguous sequence {} for `{process}` was deduped",
                    *sequence
                ));
            }
            model.live_wakes.entry(key).or_default().insert(
                *sequence,
                ExpectedQueuedWake {
                    wake,
                    delivery_policy: draft.delivery_policy,
                    slot_policy: draft.slot_policy,
                    merge_key: draft.merge_key,
                    available_at_ms: draft.available_at_ms,
                },
            );
            *sequence = sequence.saturating_add(1);
            shape.enqueues_committed = shape.enqueues_committed.saturating_add(1);
        }
        StoreContractOp::ConsumeWake {
            selection,
            highest_in_group,
            stale,
        } => {
            let Some((key, sequence)) = select_live_wake(model, *selection, *highest_in_group)
            else {
                return Ok(());
            };
            let lower_live = model
                .live_wakes
                .get(&key)
                .is_some_and(|wakes| wakes.keys().any(|candidate| *candidate < sequence));
            if consume_wake(handles.runtime.as_ref(), &key.1, sequence, *stale).await? {
                let wakes = model
                    .live_wakes
                    .get_mut(&key)
                    .expect("selected live wake group exists");
                wakes.remove(&sequence);
                shape.consumes_committed = shape.consumes_committed.saturating_add(1);
                if lower_live {
                    shape.out_of_order_states = shape.out_of_order_states.saturating_add(1);
                }
            }
        }
        StoreContractOp::Prune { watermark } => {
            let (_, cursor) = handles
                .registry
                .processes_changed_since(ProcessChangeCursor::initial(), 1_000)
                .await
                .map_err(|error| error.to_string())?;
            model.projection_cursor = cursor;
            let watermark = if *watermark {
                ProjectionWatermark::UpTo(cursor)
            } else {
                ProjectionWatermark::NoProjector
            };
            handles
                .registry
                .prune_terminal_processes(u64::MAX, None, watermark)
                .await
                .map_err(|error| error.to_string())?;
            for (id, process) in &mut model.processes {
                let pruned = matches!(
                    handles.registry.get_process(id).await,
                    Err(crate::PluginError::ProcessNoLongerRetained { .. })
                );
                if pruned {
                    if !process.tombstoned
                        && !process
                            .expected_record
                            .as_ref()
                            .is_some_and(ProcessRecord::is_terminal)
                    {
                        return Err(format!(
                            "Prune/tombstone safety: live process `{id}` was pruned"
                        ));
                    }
                    if !process.tombstoned {
                        process.reset_to_tombstone();
                    }
                }
            }
        }
        StoreContractOp::CompactTombstones { caught_up } => {
            let watermark = if *caught_up {
                let (_, cursor) = handles
                    .registry
                    .processes_changed_since(model.projection_cursor, 1_000)
                    .await
                    .map_err(|error| error.to_string())?;
                ProjectionWatermark::UpTo(cursor)
            } else {
                ProjectionWatermark::UpTo(model.projection_cursor)
            };
            handles
                .registry
                .compact_process_tombstones(u64::MAX, watermark, None)
                .await
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn selected_authority(
    model: &ReferenceModel,
    id: &str,
    stale: bool,
) -> ProcessExecutionWriteAuthority {
    if let Some(process) = model.processes.get(id) {
        if stale && let Some(authority) = process.superseded_authorities.last() {
            return authority.clone();
        }
        if !stale && let Some(authority) = process.current_authority.clone() {
            return authority;
        }
    }
    stale_authority(id)
}

fn has_current_authority(model: &ReferenceModel, id: &str) -> bool {
    model
        .processes
        .get(id)
        .and_then(|process| process.current_authority.as_ref())
        .is_some()
}

fn assert_typed_stale_authority_rejection<T>(
    result: &Result<T, crate::PluginError>,
    must_reject: bool,
    id: &str,
    operation: &str,
) -> Result<(), String> {
    if must_reject
        && !matches!(
            result,
            Err(crate::PluginError::ProcessLeaseSuperseded { process_id })
                if process_id == id
        )
    {
        return Err(format!(
            "Stale-authority non-mutation: superseded authority {operation} for `{id}` did not return ProcessLeaseSuperseded"
        ));
    }
    Ok(())
}

fn select_live_wake(
    model: &ReferenceModel,
    selection: u8,
    highest_in_group: bool,
) -> Option<((String, String), u64)> {
    if highest_in_group
        && let Some((key, wakes)) = model.live_wakes.iter().find(|(_, wakes)| wakes.len() > 1)
    {
        return wakes
            .last_key_value()
            .map(|(sequence, _)| (key.clone(), *sequence));
    }
    let live = model
        .live_wakes
        .iter()
        .flat_map(|(key, wakes)| wakes.keys().map(|sequence| (key.clone(), *sequence)))
        .collect::<Vec<_>>();
    live.get(usize::from(selection) % live.len().max(1))
        .cloned()
}

async fn process_lease_snapshot(
    registry: &Arc<dyn ProcessRegistry>,
    id: &str,
) -> Result<Option<serde_json::Value>, String> {
    registry
        .get_process_lease(id)
        .await
        .map_err(|error| error.to_string())?
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| error.to_string())
}

fn normalize_record(mut record: ProcessRecord) -> ProcessRecord {
    // Backend clocks are intentionally not synchronized. The independent model
    // pins every semantic record field; the fold law separately pins timestamps
    // by reconstructing them from the persisted event log.
    record.created_at_ms = 0;
    record.updated_at_ms = 0;
    record
}

fn terminal_output(index: u8) -> ProcessAwaitOutput {
    match index % 4 {
        0 => ProcessAwaitOutput::Success {
            value: serde_json::json!({"property": true}),
            control: None,
        },
        1 => ProcessAwaitOutput::Failure {
            class: crate::ToolFailureClass::External,
            code: "property_failure".to_string(),
            message: "generated failure".to_string(),
            raw: None,
            control: None,
        },
        2 => ProcessAwaitOutput::Cancelled {
            message: "generated cancellation".to_string(),
            raw: None,
            control: None,
        },
        _ => ProcessAwaitOutput::Abandoned {
            evidence: Box::new(crate::AbandonEvidence {
                writer: crate::AbandonWriter::EngineGaveUp,
                owner: None,
                epoch_ms: 1,
            }),
            control: None,
        },
    }
}

#[derive(Clone, Copy)]
enum WakeSettle {
    Mark,
    Discard,
    Defer,
}

async fn settle_wake(
    handles: &StoreContractHandles,
    model: &mut ReferenceModel,
    stale: bool,
    settle: WakeSettle,
) -> Result<(), String> {
    let Some(delivery) = model
        .wake_deliveries
        .values()
        .rev()
        .find(|delivery| delivery.state == WakeDeliveryState::Enqueuing)
        .cloned()
    else {
        return Ok(());
    };
    let token = delivery.claim_token().map_err(|error| error.to_string())?;
    let token = if stale {
        format!("stale-{token}")
    } else {
        token.to_string()
    };
    let before = wake_delivery_snapshot(&handles.registry, &delivery.delivery_id).await?;
    let outcome = match settle {
        WakeSettle::Mark => {
            handles
                .registry
                .mark_wake_enqueued(&delivery.delivery_id, &token)
                .await
        }
        WakeSettle::Discard => {
            handles
                .registry
                .discard_wake_delivery(&delivery.delivery_id, &token, WakeDiscardReason::TargetGone)
                .await
        }
        WakeSettle::Defer => {
            handles
                .registry
                .defer_wake_delivery(
                    &delivery.delivery_id,
                    &token,
                    delivery.next_attempt_at_ms.saturating_add(1),
                )
                .await
        }
    }
    .map_err(|error| error.to_string())?;
    if stale {
        if matches!(outcome, WakeDeliveryClaimOutcome::Applied) {
            return Err("Stale-authority non-mutation: stale wake claim was applied".to_string());
        }
        let after = wake_delivery_snapshot(&handles.registry, &delivery.delivery_id).await?;
        if before != after {
            return Err(
                "Stale-authority non-mutation: stale wake claim mutated its delivery".to_string(),
            );
        }
    } else if matches!(outcome, WakeDeliveryClaimOutcome::Applied) {
        let expected = model
            .wake_deliveries
            .get_mut(&delivery.delivery_id)
            .expect("claimed delivery is modeled");
        expected.claim_token = None;
        match settle {
            WakeSettle::Mark => {
                expected.state = WakeDeliveryState::Enqueued;
                expected.discard_reason = None;
            }
            WakeSettle::Discard => {
                expected.state = WakeDeliveryState::Discarded;
                expected.discard_reason = Some(WakeDiscardReason::TargetGone);
            }
            WakeSettle::Defer => {
                expected.state = WakeDeliveryState::Pending;
                expected.next_attempt_at_ms = delivery.next_attempt_at_ms.saturating_add(1);
            }
        }
    }
    Ok(())
}

async fn wake_delivery_snapshot(
    registry: &Arc<dyn ProcessRegistry>,
    delivery_id: &str,
) -> Result<Option<serde_json::Value>, String> {
    registry
        .list_wake_deliveries(None)
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|delivery| delivery.delivery_id == delivery_id)
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| error.to_string())
}

async fn assert_rejected_write_is_noop(
    registry: &Arc<dyn ProcessRegistry>,
    id: &str,
    before: serde_json::Value,
    rejected: bool,
    law: &str,
) -> Result<(), String> {
    if rejected {
        let after = registry_snapshot(registry, id).await;
        if before != after {
            return Err(format!("{law}: rejected operation mutated `{id}`"));
        }
    }
    Ok(())
}

async fn registry_snapshot(registry: &Arc<dyn ProcessRegistry>, id: &str) -> serde_json::Value {
    let record = registry.get_process(id).await.ok().flatten();
    let events = registry.events_after(id, 0).await.unwrap_or_default();
    let observers = registry.observers_for_process(id).await.unwrap_or_default();
    serde_json::json!({"record": record, "events": events, "observers": observers})
}

async fn assert_fold_law(
    registry: &Arc<dyn ProcessRegistry>,
    model: &ReferenceModel,
) -> Result<(), String> {
    let process_ids = model.processes.keys().cloned().collect::<Vec<_>>();
    for id in process_ids {
        let Some(base) = model
            .processes
            .get(&id)
            .and_then(|process| process.base.clone())
        else {
            continue;
        };
        let stored = registry
            .get_process(&id)
            .await
            .map_err(|error| format!("live modeled process `{id}` became unavailable: {error}"))?
            .ok_or_else(|| format!("live modeled process `{id}` disappeared"))?;
        let events = registry
            .events_after(&id, 0)
            .await
            .map_err(|error| error.to_string())?;
        let folded = fold_process_record(base, &events).map_err(|error| error.to_string())?;
        if folded != stored {
            return Err(format!(
                "stored record for `{id}` differs from fold_process_record(events_after(0))"
            ));
        }
    }
    Ok(())
}

async fn assert_model_agreement(
    handles: &StoreContractHandles,
    model: &ReferenceModel,
) -> Result<(), String> {
    for (id, expected) in &model.processes {
        if expected.tombstoned {
            if matches!(handles.registry.get_process(id).await, Ok(Some(_))) {
                return Err(format!(
                    "tombstoned process `{id}` unexpectedly became live"
                ));
            }
            continue;
        }
        let Some(expected_record) = expected.expected_record.clone() else {
            continue;
        };
        let actual_record = handles
            .registry
            .get_process(id)
            .await
            .map_err(|error| format!("modeled live process `{id}` lookup failed: {error}"))?
            .ok_or_else(|| format!("modeled live process `{id}` was absent"))?;
        if normalize_record(expected_record) != normalize_record(actual_record) {
            return Err(format!(
                "process record for `{id}` differs from the independently-derived reference model"
            ));
        }
        let actual = handles
            .registry
            .observers_for_process(id)
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .collect::<BTreeSet<_>>();
        if actual != expected.observers {
            return Err(format!(
                "observer set for `{id}` differs from reference model"
            ));
        }
    }
    let mut actual_deliveries = handles
        .registry
        .list_wake_deliveries(None)
        .await
        .map_err(|error| error.to_string())?;
    actual_deliveries.sort_by(|left, right| left.delivery_id.cmp(&right.delivery_id));
    let expected_deliveries = model.wake_deliveries.values().cloned().collect::<Vec<_>>();
    if actual_deliveries != expected_deliveries {
        return Err(format!(
            "wake delivery states differ from reference model: actual={actual_deliveries:?}, expected={expected_deliveries:?}"
        ));
    }
    let queued = handles
        .runtime
        .list_queued_work("prop-runtime-session")
        .await
        .map_err(|error| error.to_string())?;
    let mut actual_live = BTreeMap::<(String, String), BTreeMap<u64, ExpectedQueuedWake>>::new();
    for batch in queued {
        for item in batch.items {
            if let QueuedWorkPayload::ProcessWake { wake } = item.payload {
                actual_live
                    .entry((batch.session_id.clone(), wake.process_id.clone()))
                    .or_default()
                    .insert(
                        wake.sequence,
                        ExpectedQueuedWake {
                            wake: *wake,
                            delivery_policy: batch.delivery_policy,
                            slot_policy: batch.slot_policy,
                            merge_key: batch.merge_key.clone(),
                            available_at_ms: batch.available_at_ms,
                        },
                    );
            }
        }
    }
    let expected_live = model
        .live_wakes
        .iter()
        .filter(|(_, wakes)| !wakes.is_empty())
        .map(|(key, wakes)| (key.clone(), wakes.clone()))
        .collect::<BTreeMap<_, _>>();
    if actual_live != expected_live {
        return Err(format!(
            "Enqueued-wake high-water safety: live wake payload/batch state differs; actual={actual_live:?}, expected={expected_live:?}"
        ));
    }
    Ok(())
}

async fn assert_replay_key_idempotency(
    registry: &Arc<dyn ProcessRegistry>,
) -> Result<(), TestCaseError> {
    let id = "law-replay-key";
    registry
        .register_process(registration(
            id,
            RecoveryDisposition::ExternallyOwned,
            1,
            None,
        ))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let request =
        ProcessEventAppendRequest::new("property.signal", serde_json::json!({"value": 1}))
            .with_replay_key("law-replay-key:stable");
    let first = registry
        .append_event(id, request.clone())
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let replay = registry
        .append_event(id, request)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(
        serde_json::to_value(first.event)
            .map_err(|error| TestCaseError::fail(error.to_string()))?,
        serde_json::to_value(replay.event)
            .map_err(|error| TestCaseError::fail(error.to_string()))?,
        "Replay-key idempotency: identical retry returned a different event"
    );
    prop_assert_eq!(
        registry
            .events_after(id, 0)
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?
            .len(),
        1
    );
    let conflict = registry
        .append_event(
            id,
            ProcessEventAppendRequest::new("property.signal", serde_json::json!({"value": 2}))
                .with_replay_key("law-replay-key:stable"),
        )
        .await;
    prop_assert!(
        conflict.is_err(),
        "Replay-key idempotency: same key with a different payload must conflict"
    );
    prop_assert_eq!(
        registry
            .events_after(id, 0)
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?
            .len(),
        1
    );
    Ok(())
}

async fn assert_attempt_monotonicity_and_budget(
    registry: &Arc<dyn ProcessRegistry>,
) -> Result<(), TestCaseError> {
    let rerunnable = "law-attempt-budget";
    registry
        .register_process(registration(
            rerunnable,
            RecoveryDisposition::Rerunnable,
            2,
            None,
        ))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    for attempt in 1..=2 {
        let authority = invocation_authority(rerunnable, attempt as u8, attempt);
        let started = authority.invocation_started().expect("bound invocation");
        let outcome = registry
            .record_first_started_with_authority(rerunnable, started, &authority)
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        prop_assert!(matches!(outcome, ProcessStartOutcome::Started(_)));
    }
    let authority = invocation_authority(rerunnable, 3, 3);
    let exhausted = registry
        .record_first_started_with_authority(
            rerunnable,
            authority.invocation_started().expect("bound invocation"),
            &authority,
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        matches!(
            exhausted,
            ProcessStartOutcome::AttemptsExhausted {
                attempts: 2,
                max_attempts: 2,
                ..
            }
        ),
        "Attempt monotonicity / budget: max_attempts was not honored"
    );

    let owner_bound = "law-resume-only";
    registry
        .register_process(registration(
            owner_bound,
            RecoveryDisposition::OwnerBound,
            3,
            None,
        ))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let first_authority = invocation_authority(owner_bound, 1, 1);
    registry
        .record_first_started_with_authority(
            owner_bound,
            first_authority
                .invocation_started()
                .expect("bound invocation"),
            &first_authority,
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let second_authority = invocation_authority(owner_bound, 2, 2);
    let second = registry
        .record_first_started_with_authority(
            owner_bound,
            second_authority
                .invocation_started()
                .expect("bound invocation"),
            &second_authority,
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        matches!(second, ProcessStartOutcome::AlreadyStarted { .. }),
        "Attempt monotonicity / budget: OwnerBound second execution was accepted"
    );
    Ok(())
}

async fn assert_stale_authority_non_mutation(
    registry: &Arc<dyn ProcessRegistry>,
) -> Result<(), TestCaseError> {
    let id = "law-stale-authority";
    registry
        .register_process(registration(id, RecoveryDisposition::Rerunnable, 3, None))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let current = invocation_authority(id, 1, 1);
    registry
        .record_first_started_with_authority(
            id,
            current.invocation_started().expect("bound invocation"),
            &current,
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let successor = invocation_authority(id, 2, 2);
    registry
        .record_first_started_with_authority(
            id,
            successor.invocation_started().expect("bound invocation"),
            &successor,
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let before = registry_snapshot(registry, id).await;
    let stale = registry
        .append_event_with_authority(
            id,
            ProcessEventAppendRequest::new("property.signal", serde_json::json!({"stale": true}))
                .with_replay_key("law:stale"),
            &current,
        )
        .await;
    prop_assert!(
        stale.is_err(),
        "Stale-authority non-mutation: superseded attempt append succeeded"
    );
    prop_assert_eq!(
        registry_snapshot(registry, id).await,
        before,
        "Stale-authority non-mutation: superseded attempt changed durable state"
    );
    Ok(())
}

async fn assert_wake_group_order_and_claim_ownership(
    registry: &Arc<dyn ProcessRegistry>,
) -> Result<(), TestCaseError> {
    let id = "law-wake-order";
    registry
        .register_process(registration(
            id,
            RecoveryDisposition::ExternallyOwned,
            1,
            Some("law-wake-session".to_string()),
        ))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    for sequence in 1..=2 {
        registry
            .append_event(
                id,
                ProcessEventAppendRequest::new(
                    "property.wake",
                    serde_json::json!({"wake_input": sequence}),
                )
                .with_replay_key(format!("law:wake:{sequence}")),
            )
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
    }
    let first = registry
        .claim_pending_wake_deliveries(2)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(
        first.len(),
        1,
        "Wake group order + claim ownership: later group member claimed while head unsettled"
    );
    prop_assert_eq!(first[0].wake.sequence, 1);
    let competing = registry
        .claim_pending_wake_deliveries(2)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        competing.is_empty(),
        "Wake group order + claim ownership: concurrent claimant acquired an owned head"
    );
    let stale = registry
        .mark_wake_enqueued(&first[0].delivery_id, "not-the-claim-token")
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        !matches!(stale, WakeDeliveryClaimOutcome::Applied),
        "Wake group order + claim ownership: stale token settled delivery"
    );
    let applied = registry
        .mark_wake_enqueued(
            &first[0].delivery_id,
            first[0].claim_token().expect("claim token"),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(matches!(applied, WakeDeliveryClaimOutcome::Applied));
    let second = registry
        .claim_pending_wake_deliveries(2)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(second.len(), 1);
    prop_assert_eq!(second[0].wake.sequence, 2);
    Ok(())
}

async fn assert_enqueued_wake_high_water_safety(
    runtime: &Arc<dyn RuntimePersistence>,
) -> Result<(), TestCaseError> {
    let session = "law-high-water";
    let process = "law-high-water-process";
    // Consumption is intentionally not required to be contiguous: the public selected-batch
    // drain contracts safe out-of-order settlement. The production precondition is contiguous
    // enqueue, and the law is that MAX high-water advancement never removes an already-enqueued
    // lower row; literal contiguous-consumption assertions would reject that supported behavior.
    let earlier = runtime
        .enqueue_queued_work(process_wake_batch_draft(runtime_wake_for(
            session, process, 1,
        )))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let later = runtime
        .enqueue_queued_work(process_wake_batch_draft(runtime_wake_for(
            session, process, 2,
        )))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let owner = LeaseOwnerIdentity::opaque("law-high-water-owner", "law-high-water-incarnation");
    let lease = runtime
        .try_claim_session_execution_lease(session, &owner, 60_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| {
            TestCaseError::fail("Enqueued-wake high-water safety: lease unexpectedly busy")
        })?;
    let claim = runtime
        .claim_ready_queued_work_by_batch_ids(
            session,
            &lease.fence(),
            &owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&later.batch_id),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| {
            TestCaseError::fail("Enqueued-wake high-water safety: later wake was not claimable")
        })?;
    let mut state = RuntimeSessionState {
        session_id: session.to_string(),
        ..RuntimeSessionState::default()
    };
    let commit = runtime
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(lease.completion())
                .completing_queue_claim(claim.completion()),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    state.apply_persisted_commit_result(commit);
    let after_later = runtime
        .list_queued_work(session)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(
        after_later
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![earlier.batch_id.as_str()],
        "Enqueued-wake high-water safety: consuming sequence 2 removed or disturbed live sequence 1"
    );

    let redelivery = runtime
        .enqueue_queued_work(process_wake_batch_draft(runtime_wake_for(
            session, process, 2,
        )))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(
        redelivery.enqueue_seq,
        0,
        "Enqueued-wake high-water safety: consumed sequence redelivery was not deduped"
    );
    let after_redelivery = runtime
        .list_queued_work(session)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(
        after_redelivery
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![earlier.batch_id.as_str()],
        "Enqueued-wake high-water safety: synthetic redelivery receipt disturbed live sequence 1"
    );

    let owner = LeaseOwnerIdentity::opaque(
        "law-high-water-earlier-owner",
        "law-high-water-earlier-incarnation",
    );
    let lease = runtime
        .try_claim_session_execution_lease(session, &owner, 60_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .acquired()
        .ok_or_else(|| {
            TestCaseError::fail("Enqueued-wake high-water safety: second lease unexpectedly busy")
        })?;
    let claim = runtime
        .claim_ready_queued_work_by_batch_ids(
            session,
            &lease.fence(),
            &owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&earlier.batch_id),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| {
            TestCaseError::fail(
                "Enqueued-wake high-water safety: lower wake did not remain claimable",
            )
        })?;
    runtime
        .commit_runtime_state(
            RuntimeCommit::persisted_state_for_test(&state, &[])
                .releasing_session_execution_lease(lease.completion())
                .completing_queue_claim(claim.completion()),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        runtime
            .list_queued_work(session)
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?
            .is_empty(),
        "Enqueued-wake high-water safety: sequence 1 did not consume exactly once"
    );
    Ok(())
}

async fn assert_prune_tombstone_watermark_safety(
    registry: &Arc<dyn ProcessRegistry>,
) -> Result<(), TestCaseError> {
    let id = "law-prune-watermark";
    let eligible_id = "law-prune-watermark-eligible";
    let live_id = "law-prune-live-must-survive";
    registry
        .register_process(registration(
            live_id,
            RecoveryDisposition::Rerunnable,
            3,
            None,
        ))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        matches!(registry.get_process(live_id).await, Ok(Some(_))),
        "Prune/tombstone/watermark safety: live process was pruned"
    );
    registry
        .register_process(registration(
            eligible_id,
            RecoveryDisposition::ExternallyOwned,
            1,
            None,
        ))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    registry
        .complete_process(
            eligible_id,
            ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    registry
        .register_process(registration(
            id,
            RecoveryDisposition::ExternallyOwned,
            1,
            None,
        ))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let (_, before_terminal) = registry
        .processes_changed_since(ProcessChangeCursor::initial(), 1_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    registry
        .complete_process(
            id,
            ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::UpTo(before_terminal))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        matches!(registry.get_process(id).await, Ok(Some(_))),
        "Prune/tombstone/watermark safety: subject terminal was pruned before projection watermark passed it"
    );
    prop_assert!(
        matches!(
            registry.get_process(eligible_id).await,
            Err(crate::PluginError::ProcessNoLongerRetained { .. })
        ),
        "Prune/tombstone/watermark safety: terminal below the projection watermark was not eligible for pruning"
    );
    let (_, terminal_cursor) = registry
        .processes_changed_since(before_terminal, 1_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::UpTo(terminal_cursor))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let tombstone = registry.get_process(id).await;
    prop_assert!(
        matches!(
            tombstone,
            Err(crate::PluginError::ProcessNoLongerRetained { .. })
        ),
        "Prune/tombstone/watermark safety: pruned id was indistinguishable from never-existing id"
    );
    registry
        .compact_process_tombstones(u64::MAX, ProjectionWatermark::UpTo(terminal_cursor), None)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        matches!(
            registry.get_process(id).await,
            Err(crate::PluginError::ProcessNoLongerRetained { .. })
        ),
        "Prune/tombstone/watermark safety: subject tombstone compacted before its deletion was projected"
    );
    let (changes, deletion_cursor) = registry
        .processes_changed_since(terminal_cursor, 1_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let deleted = changes
        .iter()
        .find_map(|change| match change {
            ProcessChange::Deleted { tombstone } if tombstone.process_id == id => Some(tombstone),
            _ => None,
        })
        .ok_or_else(|| {
            TestCaseError::fail("Prune/tombstone/watermark safety: deletion feed omitted tombstone")
        })?;
    let encoded =
        serde_json::to_value(deleted).map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        encoded.get("outcome").is_none()
            && encoded.get("events").is_none()
            && encoded.get("input").is_none(),
        "Prune/tombstone/watermark safety: tombstone retained payload"
    );
    registry
        .compact_process_tombstones(u64::MAX, ProjectionWatermark::UpTo(deletion_cursor), None)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        matches!(registry.get_process(id).await, Ok(None)),
        "Prune/tombstone/watermark safety: projected subject tombstone was not compacted"
    );
    prop_assert!(
        matches!(registry.get_process(live_id).await, Ok(Some(_))),
        "Prune/tombstone/watermark safety: live process did not survive the full prune lifecycle"
    );
    Ok(())
}

/// Pin only the registry-row behavior accepted by the store surface.
///
/// This is not a claim that a reused process id is globally fresh: the receiver's
/// consumed wake high-water survives sender pruning, so `ProcessRegistry` explicitly
/// instructs hosts to mint unique ids across prune horizons.
async fn assert_prune_reregister_registry_state_is_fresh(
    registry: &Arc<dyn ProcessRegistry>,
) -> Result<(), TestCaseError> {
    let id = "law-prune-reregister";
    registry
        .register_process(registration(
            id,
            RecoveryDisposition::ExternallyOwned,
            1,
            Some("law-prune-reregister-old".to_string()),
        ))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    registry
        .append_event(
            id,
            ProcessEventAppendRequest::new(
                "property.signal",
                serde_json::json!({"identity": "old"}),
            )
            .with_replay_key("law:prune-reregister:old"),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    registry
        .complete_process(
            id,
            ProcessAwaitOutput::Success {
                value: serde_json::json!({"identity": "old"}),
                control: None,
            },
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let (_, terminal_cursor) = registry
        .processes_changed_since(ProcessChangeCursor::initial(), 1_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::UpTo(terminal_cursor))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        matches!(
            registry.get_process(id).await,
            Err(crate::PluginError::ProcessNoLongerRetained { .. })
        ),
        "Prune/re-register registry state: prune did not leave a tombstone"
    );
    let (_, deletion_cursor) = registry
        .processes_changed_since(terminal_cursor, 1_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;

    let fresh_base = registry
        .register_process(registration(
            id,
            RecoveryDisposition::Rerunnable,
            3,
            Some("law-prune-reregister-new".to_string()),
        ))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let live = registry
        .get_process(id)
        .await
        .map_err(|error| TestCaseError::fail(format!(
            "Prune/re-register registry state: fresh live record did not shadow stale tombstone: {error}"
        )))?
        .ok_or_else(|| {
            TestCaseError::fail("Prune/re-register registry state: fresh live record was absent")
        })?;
    let fresh_events = registry
        .events_after(id, 0)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        fresh_events.is_empty(),
        "Prune/re-register registry state: re-registered row inherited the old event log"
    );
    let folded = fold_process_record(fresh_base, &fresh_events)
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(
        folded,
        live,
        "Prune/re-register registry state: new baseline and event log did not fold to the live record"
    );

    let compacted = registry
        .compact_process_tombstones(u64::MAX, ProjectionWatermark::UpTo(deletion_cursor), None)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(
        compacted,
        1,
        "Prune/re-register registry state: stale tombstone was not independently compactable"
    );
    prop_assert!(
        matches!(registry.get_process(id).await, Ok(Some(_))),
        "Prune/re-register registry state: compacting the stale tombstone removed the re-registered row"
    );
    Ok(())
}

fn runtime_wake(process_id: &str, sequence: u64) -> ProcessWakeDelivery {
    runtime_wake_for("prop-runtime-session", process_id, sequence)
}

fn runtime_wake_for(session_id: &str, process_id: &str, sequence: u64) -> ProcessWakeDelivery {
    ProcessWakeDelivery {
        wake_id: format!("wake:{process_id}:{sequence}"),
        target_session_id: session_id.to_string(),
        process_id: process_id.to_string(),
        sequence,
        event_type: "property.wake".to_string(),
        event_invocation: RuntimeInvocation {
            scope: RuntimeScope::new(session_id),
            subject: RuntimeSubject::ProcessEvent {
                process_id: process_id.to_string(),
                sequence,
                event_type: "property.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        input: format!("wake-{sequence}"),
        created_at_ms: 1,
    }
}

async fn consume_wake(
    runtime: &dyn RuntimePersistence,
    process_id: &str,
    sequence: u64,
    stale: bool,
) -> Result<bool, String> {
    let session = "prop-runtime-session";
    let queued = runtime
        .list_queued_work(session)
        .await
        .map_err(|error| error.to_string())?;
    let Some(batch) = queued.iter().find(|batch| batch.items.iter().any(|item| matches!(
        &item.payload, QueuedWorkPayload::ProcessWake { wake } if wake.process_id == process_id && wake.sequence == sequence
    ))) else { return Ok(false); };
    let owner = LeaseOwnerIdentity::opaque("property-consumer", "property-consumer-incarnation");
    let Some(lease) = runtime
        .try_claim_session_execution_lease(session, &owner, 60_000)
        .await
        .map_err(|error| error.to_string())?
        .acquired()
    else {
        return Ok(false);
    };
    let Some(claim) = runtime
        .claim_ready_queued_work_by_batch_ids(
            session,
            &lease.fence(),
            &owner,
            QueuedWorkClaimBoundary::Idle,
            std::slice::from_ref(&batch.batch_id),
        )
        .await
        .map_err(|error| error.to_string())?
    else {
        return Ok(false);
    };
    let state = crate::load_persisted_session_state(runtime)
        .await
        .map_err(|error| error.to_string())?
        .unwrap_or_else(|| RuntimeSessionState {
            session_id: session.to_string(),
            ..RuntimeSessionState::default()
        });
    let completion = if stale {
        let mut completion = claim.completion();
        completion.lease_token = format!("stale-{}", completion.lease_token);
        completion
    } else {
        claim.completion()
    };
    let operation = crate::OperationId::new(
        crate::ExecutionScope::runtime_operation(format!(
            "store-contract-consume:{process_id}:{sequence}:{}",
            completion.lease_token
        )),
        "commit",
    );
    let before = queued_batch_snapshot(runtime, session, &batch.batch_id).await?;
    let commit = RuntimeCommit::persisted_state_for_test(&state, &[])
        .with_operation(operation)
        .map_err(|error| error.to_string())?
        .0
        .releasing_session_execution_lease(lease.completion())
        .completing_queue_claim(completion);
    let result = runtime.commit_runtime_state(commit).await;
    if stale {
        if result.is_ok() {
            return Err(
                "Stale-authority non-mutation: stale queued-work claim committed".to_string(),
            );
        }
        let after = queued_batch_snapshot(runtime, session, &batch.batch_id).await?;
        if before != after {
            return Err(
                "Stale-authority non-mutation: stale queued-work claim changed its batch"
                    .to_string(),
            );
        }
        return Ok(false);
    } else if result.is_ok() {
        return Ok(true);
    }
    result.map_err(|error| error.to_string())?;
    Ok(false)
}

async fn queued_batch_snapshot(
    runtime: &dyn RuntimePersistence,
    session_id: &str,
    batch_id: &str,
) -> Result<Option<serde_json::Value>, String> {
    runtime
        .list_queued_work(session_id)
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|batch| batch.batch_id == batch_id)
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| error.to_string())
}

fn counterexample_path(backend: &str) -> PathBuf {
    let root = std::env::var_os("LASH_STORE_CONTRACT_COUNTEREXAMPLE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("LASH_CONFIDENCE_OUT_DIR")
                .map(PathBuf::from)
                .map(|path| path.join("store-contract-counterexamples"))
        })
        .or_else(|| {
            std::env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .map(|path| path.join("store-contract-counterexamples"))
        })
        .unwrap_or_else(|| std::env::temp_dir().join("lash-store-contract-counterexamples"));
    root.join(format!("{backend}.txt"))
}

fn persist_counterexample(backend: &str, runner_seed: u64, error: &TestError<GeneratedCase>) {
    let path = counterexample_path(backend);
    if let Some(parent) = path.parent()
        && let Err(write_error) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "could not create store-contract counterexample directory {}: {write_error}",
            parent.display()
        );
        return;
    }
    let (case_seed, operations) = match error {
        TestError::Fail(_, case) => (Some(case.seed), Some(&case.operations)),
        TestError::Abort(_) => (None, None),
    };
    let body = format!(
        "backend: {backend}\nproptest_runner_seed: {runner_seed}\ncase_seed: {case_seed:?}\nminimal_operations: {operations:#?}\nfailure: {error}\n"
    );
    match std::fs::write(&path, body) {
        Ok(()) => eprintln!(
            "persisted minimized store-contract counterexample to {}",
            path.display()
        ),
        Err(write_error) => eprintln!(
            "could not persist store-contract counterexample to {}: {write_error}",
            path.display()
        ),
    }
}
