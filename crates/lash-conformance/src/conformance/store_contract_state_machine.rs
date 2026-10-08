//! Model-based property laws for the durable process and runtime stores.
//!
//! The generator lives here so every backend replays exactly the same operation
//! language through the public trait-object contracts. Backend crates only
//! provide fresh handles; they do not carry a `proptest` dependency.
use super::process_references::{ProcessCountConservation, assert_process_count_conservation};
use super::*;
use crate::ProcessEventLogTestSupport as _;
use crate::{
    ProcessCompletionOutcome, ProcessExecutionWriteAuthority, ProcessExternalRef,
    ProcessObserverBy, ProcessRecord, ProcessStartOutcome, ProjectionWatermark,
    apply_process_event_projection, fold_process_record, process_wake_batch_draft,
};
use generated_prefix::generated_prefix;
use lash_sansio::{ProcessId, SessionId};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestError, TestRunner};
use run_shape::{RunShape, RunShapeCounter, RunShapeTotals};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
const PROCESS_COUNT: u8 = 3;
const SESSION_COUNT: u8 = 2;
const DEFAULT_CASES: u32 = 32;
const DEFAULT_RUNNER_SEED: u64 = 830;
const MAX_OPS: usize = 48;
const GENERATED_PREFIX_OPS: usize = 11;
const DEDICATED_LAW_SEED: u64 = 0xded1_ca7e;
mod counterexamples;
use counterexamples::persist_counterexample;
mod event_sequence_floors;
use event_sequence_floors::EventSequenceStep;
mod generated_prefix;
mod generator;
mod model_agreement;
mod run_shape;
use generator::generated_case;
pub use generator::sample_store_contract_operations;
use lash_core::PROCESS_WAKE_DELIVERY_FORMAT_VERSION;
use model_agreement::{assert_model_agreement, terminal_outcome_under_standing_cancel};
/// Fresh process-registry and runtime-persistence handles for one generated case.
pub struct StoreContractHandles {
    pub registry: Arc<dyn ProcessRegistry>,
    pub runtime: Arc<dyn RuntimeStore>,
}
/// The generated operation alphabet shared by every durable store backend.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum StoreContractOp {
    Register {
        process: u8,
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
        requester: u8,
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
    EnqueueWake {
        process: u8,
    },
    ConsumeWake {
        selection: u8,
        highest_in_group: bool,
    },
    Prune {
        watermark: bool,
    },
    CompactTombstones {
        caught_up: bool,
    },
}

/// Stateful driver for the shared generated store-contract operation language.
/// This deliberately performs only the operation semantics and the small
/// amount of bookkeeping needed by later operations (current authorities,
/// wake claims, and queue selections). The property harness layers its
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

    /// The process a generated operation on `index` addresses: the slot's
    /// current run, or an id no registrar minted while the slot is unused.
    pub fn slot_process_id(&self, index: u8) -> ProcessId {
        self.model.slot_id(index)
    }
}

#[derive(Clone, Debug, serde::Deserialize)]
struct GeneratedCase {
    seed: u64,
    operations: Vec<StoreContractOp>,
}

/// The lifecycle of a modeled process: the only three legal combinations of
/// the old `base`/`expected_record`/`tombstoned` triple.
#[derive(Clone, Debug, Default)]
enum ProcessLifecycle {
    /// Never registered, or only touched by operations that did not spawn it.
    #[default]
    Absent,
    /// Registered and retained: `base` is the record the fold law replays
    /// events onto, `expected` is the independently derived projection.
    Live {
        base: Box<ProcessRecord>,
        expected: Box<ProcessRecord>,
    },
    /// Pruned to a tombstone; a later register of its slot starts a new run.
    Tombstoned,
}

#[derive(Clone, Debug, Default)]
struct ModelProcess {
    lifecycle: ProcessLifecycle,
    wake_target: Option<SessionId>,
    observers: BTreeSet<SessionId>,
    lifecycle_replay_keys: BTreeSet<String>,
    current_authority: Option<ProcessExecutionWriteAuthority>,
    superseded_authorities: Vec<ProcessExecutionWriteAuthority>,
}

#[derive(Clone, Debug, PartialEq)]
struct ExpectedQueuedWake {
    wake: crate::ProcessWakeDelivery,
    delivery_policy: DeliveryPolicy,
    kind: crate::QueuedWorkKind,
    authority: crate::QueuedWorkAuthority,
    merge_key: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct ReferenceModel {
    /// The id the registrar minted for each generated process slot's current
    /// run. A slot re-registered after its run was pruned names a new run.
    slot_ids: BTreeMap<u8, ProcessId>,
    processes: BTreeMap<ProcessId, ModelProcess>,
    live_wakes: BTreeMap<(SessionId, ProcessId), BTreeMap<u64, ExpectedQueuedWake>>,
    next_wake_sequence: BTreeMap<(SessionId, ProcessId), u64>,
    projection_cursor: ProcessChangeCursor,
    process_counts: ProcessCountConservation,
}

impl ReferenceModel {
    /// The process a generated operation on `index` addresses: the slot's
    /// current run, or an id no registrar minted while the slot is unused.
    fn slot_id(&self, index: u8) -> ProcessId {
        let slot = index % PROCESS_COUNT;
        self.slot_ids
            .get(&slot)
            .cloned()
            .unwrap_or_else(|| unregistered_slot_id(slot))
    }

    fn process_mut(&mut self, id: &ProcessId) -> &mut ModelProcess {
        self.processes.entry(id.clone()).or_default()
    }
}

impl ModelProcess {
    fn is_tombstoned(&self) -> bool {
        matches!(self.lifecycle, ProcessLifecycle::Tombstoned)
    }

    fn is_live(&self) -> bool {
        matches!(self.lifecycle, ProcessLifecycle::Live { .. })
    }

    fn expected(&self) -> Option<&ProcessRecord> {
        match &self.lifecycle {
            ProcessLifecycle::Live { expected, .. } => Some(expected.as_ref()),
            _ => None,
        }
    }

    fn expected_mut(&mut self) -> Option<&mut ProcessRecord> {
        match &mut self.lifecycle {
            ProcessLifecycle::Live { expected, .. } => Some(expected.as_mut()),
            _ => None,
        }
    }

    fn reset_to_tombstone(&mut self) {
        *self = Self {
            lifecycle: ProcessLifecycle::Tombstoned,
            ..Self::default()
        };
    }

    fn install_fresh(&mut self, record: ProcessRecord) {
        *self = Self {
            lifecycle: ProcessLifecycle::Live {
                base: Box::new(record.clone()),
                expected: Box::new(record),
            },
            ..Self::default()
        };
    }
}

async fn cursor_after_full_relist_if_required(
    registry: &Arc<dyn ProcessRegistry>,
) -> Result<ProcessChangeCursor, String> {
    match registry
        .processes_changed_since(ProcessChangeCursor::initial(), 1_000)
        .await
    {
        Ok((_, cursor)) => Ok(cursor),
        Err(crate::PluginError::ProcessChangeCursorPruned {
            tombstone_compaction_horizon,
            ..
        }) => {
            registry
                .list_processes(&crate::ProcessListFilter::default())
                .await
                .map_err(|error| error.to_string())?;
            registry
                .processes_changed_since(tombstone_compaction_horizon, 1_000)
                .await
                .map(|(_, cursor)| cursor)
                .map_err(|error| error.to_string())
        }
        Err(error) => Err(error.to_string()),
    }
}

/// Run the named store-contract laws with proptest shrinking.
///
/// On failure this writes the case seed and the minimized operation trace before
/// panicking, so a backend defect remains reproducible even when the test log is
/// unavailable.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn store_contract_state_machine<F, Fut>(backend: &'static str, make: F)
where
    F: Fn(u64, String) -> Fut + Send + Sync + Clone + 'static,
    Fut: Future<Output = StoreContractHandles> + Send + 'static,
{
    let first = make(u64::MAX - 2, "prop-runtime-session".to_string()).await;
    let second = make(u64::MAX - 2, "prop-runtime-session".to_string()).await;
    assert!(
        !Arc::ptr_eq(&first.registry, &second.registry),
        "store_contract_state_machine reused one process-registry Arc"
    );
    assert!(
        !Arc::ptr_eq(&first.runtime, &second.runtime),
        "store_contract_state_machine reused one runtime-persistence Arc"
    );
    drop((first, second));
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
                let handles = make(case.seed, "prop-runtime-session".to_string()).await;
                let shape = replay_case(handles, &case.operations).await?;
                prop_assert!(
                    shape[RunShapeCounter::ConsumesCommitted] > 0,
                    "generated alphabet starvation: case committed no wake consumes"
                );
                prop_assert!(
                    shape[RunShapeCounter::OutOfOrderStates] > 0,
                    "generated alphabet starvation: case reached no out-of-order settlement state"
                );
                runner_shape_totals.add(&shape);
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
        "store-contract run shape ({backend}, cases={cases}): {}",
        shape_totals.report()
    );
}

async fn replay_case(
    handles: StoreContractHandles,
    operations: &[StoreContractOp],
) -> Result<RunShape, TestCaseError> {
    let mut scenario = StoreContractScenario::new(handles);
    for (step, operation) in operations.iter().enumerate() {
        let terminal_transitions_before = scenario.shape[RunShapeCounter::TerminalTransitions];
        let prune_ops_with_effect_before = scenario.shape[RunShapeCounter::PruneOpsWithEffect];
        scenario.apply(operation).await.map_err(|reason| {
            TestCaseError::fail(format!("step {step} {operation:?}: {reason}"))
        })?;
        if step >= GENERATED_PREFIX_OPS {
            scenario.shape[RunShapeCounter::TailTerminalTransitions] =
                scenario.shape[RunShapeCounter::TailTerminalTransitions].saturating_add(
                    scenario.shape[RunShapeCounter::TerminalTransitions]
                        .saturating_sub(terminal_transitions_before),
                );
            if matches!(operation, StoreContractOp::Prune { .. }) {
                scenario.shape[RunShapeCounter::TailPruneOps] =
                    scenario.shape[RunShapeCounter::TailPruneOps].saturating_add(1);
                scenario.shape[RunShapeCounter::TailPruneOpsWithEffect] =
                    scenario.shape[RunShapeCounter::TailPruneOpsWithEffect].saturating_add(
                        scenario.shape[RunShapeCounter::PruneOpsWithEffect]
                            .saturating_sub(prune_ops_with_effect_before),
                    );
            }
        }
        assert_fold_law(&scenario.handles.registry, &scenario.model)
            .await
            .map_err(|reason| TestCaseError::fail(format!("Fold at step {step}: {reason}")))?;
        assert_process_count_conservation(
            &scenario.handles.registry,
            scenario.model.process_counts,
        )
        .await
        .map_err(TestCaseError::fail)?;
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
    F: Fn(u64, String) -> Fut,
    Fut: Future<Output = StoreContractHandles>,
{
    let cases: Vec<GeneratedCase> =
        serde_json::from_str(include_str!("store_contract_regressions.json"))
            .map_err(|error| TestCaseError::fail(format!("invalid regression corpus: {error}")))?;
    for (index, case) in cases.iter().enumerate() {
        let handles = make(case.seed, "prop-runtime-session".to_string()).await;
        replay_case(handles, &case.operations)
            .await
            .map_err(|reason| TestCaseError::fail(format!("regression case {index}: {reason}")))?;
    }
    Ok(())
}

async fn assert_dedicated_laws<F, Fut>(make: &F, seed: u64) -> Result<(), TestCaseError>
where
    F: Fn(u64, String) -> Fut,
    Fut: Future<Output = StoreContractHandles>,
{
    assert_on_fresh_handles(
        make,
        seed,
        &SessionId::from("prop-runtime-session"),
        |handles| async move { assert_replay_key_idempotency(&handles.registry).await },
    )
    .await?;
    assert_on_fresh_handles(
        make,
        seed,
        &SessionId::from("prop-runtime-session"),
        |handles| async move { assert_attempt_monotonicity(&handles.registry).await },
    )
    .await?;
    assert_on_fresh_handles(
        make,
        seed,
        &SessionId::from("prop-runtime-session"),
        |handles| async move { assert_stale_authority_non_mutation(&handles.registry).await },
    )
    .await?;
    assert_on_fresh_handles(
        make,
        seed,
        &SessionId::from("law-high-water"),
        |handles| async move { assert_enqueued_wake_high_water_safety(&handles.runtime).await },
    )
    .await?;
    assert_on_fresh_handles(
        make,
        seed,
        &SessionId::from("law-prune-wake"),
        |handles| async move { assert_prune_reregister_wake_names_the_new_run(&handles).await },
    )
    .await?;
    assert_on_fresh_handles(
        make,
        seed,
        &SessionId::from("prop-runtime-session"),
        |handles| async move { assert_prune_tombstone_watermark_safety(&handles.registry).await },
    )
    .await?;
    assert_on_fresh_handles(
        make,
        seed,
        &SessionId::from("prop-runtime-session"),
        |handles| async move {
            assert_prune_reregister_registry_state_is_fresh(&handles.registry).await
        },
    )
    .await
}

async fn assert_on_fresh_handles<F, Fut, Law, LawFut>(
    make: &F,
    seed: u64,
    session_id: &SessionId,
    law: Law,
) -> Result<(), TestCaseError>
where
    F: Fn(u64, String) -> Fut,
    Fut: Future<Output = StoreContractHandles>,
    Law: FnOnce(StoreContractHandles) -> LawFut,
    LawFut: Future<Output = Result<(), TestCaseError>>,
{
    // Dedicated laws always construct handles here; generated-run handles never enter this path.
    law(make(seed, session_id.to_string()).await).await
}

/// The start key of a generated process slot: re-registering a retained run
/// returns it, and a slot whose run was pruned starts a new one.
fn slot_start_key(slot: u8) -> crate::StartKey {
    crate::StartKey::for_host(format!("prop-process-{slot}"))
}

/// The id an operation names before its slot was ever registered.
fn unregistered_slot_id(slot: u8) -> ProcessId {
    crate::ProcessId::fixture(&format!("prop-process-{slot}"))
}

fn session_id(index: u8) -> SessionId {
    SessionId::fixture(format!("prop-session-{}", index % SESSION_COUNT))
}

/// A generated process: an `Engine` input with its execution env.
fn registration(label: &str, wake_target: Option<SessionId>) -> ProcessRegistration {
    let registration = ProcessRegistration::new(
        ProcessInput::Engine {
            kind: "store-contract-property".to_string(),
            payload: serde_json::json!({"label": label}),
        },
        ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_execution_env_ref(Some(lash_core::testing::process_execution_env_fixture_ref()));
    registration
        .with_extra_event_types([
            ProcessEventType {
                name: "property.signal".to_string(),
                payload_schema: JsonSchema::any(),
                semantics: ProcessEventSemanticsSpec::default(),
            },
            ProcessEventType {
                name: "property.wake".to_string(),
                payload_schema: JsonSchema::any(),
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
    process_id: &ProcessId,
    owner: u8,
    attempt: u32,
) -> ProcessExecutionWriteAuthority {
    ProcessExecutionWriteAuthority::invocation(
        process_id,
        format!("owner-{owner}-attempt-{attempt}"),
    )
    .bind_attempt(attempt)
}

fn stale_authority(process_id: &ProcessId) -> ProcessExecutionWriteAuthority {
    invocation_authority(process_id, 250, 250)
}

fn wait_state(process_id: &ProcessId) -> WaitState {
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

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn apply_operation(
    handles: &StoreContractHandles,
    model: &mut ReferenceModel,
    shape: &mut RunShape,
    operation: &StoreContractOp,
) -> Result<(), String> {
    let event_sequences = EventSequenceStep::capture(model);
    match operation {
        StoreContractOp::Register {
            process,
            wake_target,
        } => {
            let slot = *process % PROCESS_COUNT;
            let target = wake_target.map(session_id);
            let result = handles
                .registry
                .register_process(
                    registration(&format!("prop-process-{slot}"), target.clone())
                        .with_start_key(Some(slot_start_key(slot))),
                )
                .await;
            if let Ok(record) = result {
                let id = record.id.clone();
                model.slot_ids.insert(slot, id.clone());
                let entry = model.process_mut(&id);
                // The slot's start key returns its retained run; once that run
                // is pruned the key starts a new run under a new id (ADR 0107).
                if !entry.is_live() {
                    entry.install_fresh(record);
                    entry.wake_target = target;
                    model.process_counts.record_spawn();
                    shape[RunShapeCounter::Spawns] =
                        shape[RunShapeCounter::Spawns].saturating_add(1);
                }
            }
        }
        StoreContractOp::FirstStart {
            process,
            owner,
            attempt,
        } => {
            let id = model.slot_id(*process);
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
                if let Some(expected) = entry.expected_mut() {
                    event_sequences.advance(expected);
                    expected.first_started = Some(Box::new(started));
                }
            }
        }
        StoreContractOp::EnterWait { process, stale } => {
            let id = model.slot_id(*process);
            let authority = selected_authority(model, &id, *stale);
            let must_reject = *stale && has_current_authority(model, &id);
            let before = registry_snapshot(&handles.registry, &id).await;
            let result = handles
                .registry
                .set_process_wait_with_authority(&id, wait_state(&id), Vec::new(), &authority)
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
                && let Some(expected) = model.process_mut(&id).expected_mut()
            {
                if expected.wait() != Some(&wait_state(&id)) {
                    event_sequences.advance(expected);
                }
                expected.lifecycle = crate::ProcessLifecycleState::Waiting {
                    wait: wait_state(&id),
                };
            }
        }
        StoreContractOp::ClearWait { process, stale } => {
            let id = model.slot_id(*process);
            let authority = selected_authority(model, &id, *stale);
            let must_reject = *stale && has_current_authority(model, &id);
            let before = registry_snapshot(&handles.registry, &id).await;
            let result = handles
                .registry
                .clear_process_wait_with_authority(&id, Vec::new(), &authority)
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
                && let Some(expected) = model.process_mut(&id).expected_mut()
                && expected.wait().is_some()
            {
                event_sequences.advance(expected);
                expected.lifecycle = crate::ProcessLifecycleState::running();
            }
        }
        StoreContractOp::SetExternalRef { process, value } => {
            let id = model.slot_id(*process);
            let external_ref = ProcessExternalRef {
                backend: "property".to_string(),
                id: format!("external-{value}"),
                metadata: None,
                segment_ordinal: None,
            };
            if handles
                .registry
                .set_external_ref(&id, external_ref.clone())
                .await
                .is_ok()
                && let Some(expected) = model.process_mut(&id).expected_mut()
                && expected.external_ref.is_none()
            {
                event_sequences.advance(expected);
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
            let id = model.slot_id(*process);
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
            if let Ok(appended) = result
                && let Some(expected) = model.process_mut(&id).expected_mut()
            {
                apply_process_event_projection(expected, &appended.event)
                    .map_err(|error| error.to_string())?;
                expected.last_event_sequence = appended.last_event_sequence;
            }
        }
        StoreContractOp::CancelRequest { process, requester } => {
            let id = model.slot_id(*process);
            if let Ok(process_id) = handles.registry.require_process_id(&id).await
                && let Ok(appended) = handles
                    .registry
                    .append_event(
                        &process_id,
                        ProcessEventAppendRequest::cancel_requested(
                            &process_id,
                            &lash_core::CancelRequest::new(
                                lash_core::CancelOrigin::OperatorRequested,
                                format!("actor:state-machine:{requester}"),
                                11,
                            ),
                        ),
                    )
                    .await
                && let Some(expected) = model.process_mut(&id).expected_mut()
            {
                apply_process_event_projection(expected, &appended.event)
                    .map_err(|error| error.to_string())?;
                expected.last_event_sequence = appended.last_event_sequence;
            }
        }
        StoreContractOp::Terminal {
            process,
            disposition: terminal,
        } => {
            let id = model.slot_id(*process);
            let output = terminal_output(*terminal);
            if let Ok(Some(record)) = handles.registry.get_process(&id).await {
                let authority =
                    ProcessCompletionAuthority::workflow_key(format!("property:{}", record.id));
                if let Ok(ProcessCompletionOutcome::Committed(_)) = handles
                    .registry
                    .complete_process(&id, output.clone(), authority)
                    .await
                {
                    shape[RunShapeCounter::TerminalTransitions] =
                        shape[RunShapeCounter::TerminalTransitions].saturating_add(1);
                    if let Some(expected) = model.process_mut(&id).expected_mut() {
                        event_sequences.advance(expected);
                        let settled = terminal_outcome_under_standing_cancel(
                            output,
                            expected.cancel_request.as_deref(),
                        );
                        expected.lifecycle = crate::ProcessLifecycleState::Terminal {
                            outcome: settled.try_into().expect("generated output is terminal"),
                        };
                    }
                }
            }
        }
        StoreContractOp::AddObserver { process, session } => {
            let id = model.slot_id(*process);
            let session = session_id(*session);
            if handles
                .registry
                .add_observer(&session, &id, ProcessObserverBy::host("property"))
                .await
                .is_ok()
            {
                let process = model.process_mut(&id);
                if process.observers.insert(session.clone()) {
                    event_sequences.advance_lifecycle(
                        process,
                        ProcessEventAppendRequest::observer_added(
                            &id,
                            &session,
                            &ProcessObserverBy::host("property"),
                        ),
                    );
                }
            }
        }
        StoreContractOp::RemoveObserver { process, session } => {
            let id = model.slot_id(*process);
            let session = session_id(*session);
            if handles
                .registry
                .remove_observer(&session, &id, ProcessObserverBy::host("property"))
                .await
                .is_ok()
            {
                let process = model.process_mut(&id);
                if process.observers.remove(&session) {
                    event_sequences.advance_lifecycle(
                        process,
                        ProcessEventAppendRequest::observer_removed(
                            &id,
                            &session,
                            &ProcessObserverBy::host("property"),
                        ),
                    );
                }
            }
        }
        StoreContractOp::Retarget { process, session } => {
            let id = model.slot_id(*process);
            let target = session.map(session_id);
            if handles
                .registry
                .retarget_subscription(&id, target.as_deref())
                .await
                .is_ok()
            {
                let process = model.process_mut(&id);
                if process.wake_target != target {
                    process.wake_target = target.clone();
                    event_sequences.advance_lifecycle(
                        process,
                        ProcessEventAppendRequest::subscription_retargeted(&id, target.as_deref()),
                    );
                }
            }
        }
        StoreContractOp::EnqueueWake { process } => {
            let process = model.slot_id(*process);
            let key = (SessionId::from("prop-runtime-session"), process.clone());
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
                    kind: draft.kind(),
                    authority: draft.authority,
                    merge_key: draft.merge_key,
                },
            );
            *sequence = sequence
                .checked_add(1)
                .expect("generated enqueue sequence must remain in range");
            shape[RunShapeCounter::EnqueuesCommitted] =
                shape[RunShapeCounter::EnqueuesCommitted].saturating_add(1);
        }
        StoreContractOp::ConsumeWake {
            selection,
            highest_in_group,
        } => {
            let Some((key, sequence)) = select_live_wake(model, *selection, *highest_in_group)
            else {
                return Ok(());
            };
            let lower_live = model
                .live_wakes
                .get(&key)
                .is_some_and(|wakes| wakes.keys().any(|candidate| *candidate < sequence));
            if consume_wake(handles.runtime.as_ref(), &key.1, sequence).await? {
                let wakes = model
                    .live_wakes
                    .get_mut(&key)
                    .expect("selected live wake group exists");
                wakes.remove(&sequence);
                shape[RunShapeCounter::ConsumesCommitted] =
                    shape[RunShapeCounter::ConsumesCommitted].saturating_add(1);
                if lower_live {
                    shape[RunShapeCounter::OutOfOrderStates] =
                        shape[RunShapeCounter::OutOfOrderStates].saturating_add(1);
                }
            }
        }
        StoreContractOp::Prune { watermark } => {
            let cursor = cursor_after_full_relist_if_required(&handles.registry).await?;
            model.projection_cursor = cursor;
            let watermark = if *watermark {
                ProjectionWatermark::UpTo(cursor)
            } else {
                ProjectionWatermark::NoProjector
            };
            let report = handles
                .registry
                // SQL stores saturate this u64 cutoff with
                // i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX).
                .prune_terminal_processes(u64::MAX, None, watermark)
                .await
                .map_err(|error| error.to_string())?;
            model.process_counts.record_pruned(report.pruned_processes);
            if report.pruned_processes > 0 {
                shape[RunShapeCounter::PruneOpsWithEffect] =
                    shape[RunShapeCounter::PruneOpsWithEffect].saturating_add(1);
            }
            for (id, process) in &mut model.processes {
                let pruned = matches!(
                    handles.registry.get_process(id).await,
                    Err(crate::PluginError::ProcessNoLongerRetained { .. })
                );
                if pruned {
                    if !process.is_tombstoned()
                        && !process.expected().is_some_and(ProcessRecord::is_terminal)
                    {
                        return Err(format!(
                            "Prune/tombstone safety: live process `{id}` was pruned"
                        ));
                    }
                    if !process.is_tombstoned() {
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
                model.projection_cursor = cursor;
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
    id: &ProcessId,
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

fn has_current_authority(model: &ReferenceModel, id: &ProcessId) -> bool {
    model
        .processes
        .get(id)
        .and_then(|process| process.current_authority.as_ref())
        .is_some()
}

fn assert_typed_stale_authority_rejection<T>(
    result: &Result<T, crate::PluginError>,
    must_reject: bool,
    id: &ProcessId,
    operation: &str,
) -> Result<(), String> {
    if must_reject
        && !matches!(
            result,
            Err(crate::PluginError::ProcessExecutionSuperseded { process_id })
                if process_id == id
        )
    {
        return Err(format!(
            "Stale-authority non-mutation: superseded authority {operation} for `{id}` did not return ProcessExecutionSuperseded"
        ));
    }
    Ok(())
}

fn select_live_wake(
    model: &ReferenceModel,
    selection: u8,
    highest_in_group: bool,
) -> Option<((SessionId, ProcessId), u64)> {
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

fn terminal_output(index: u8) -> ProcessAwaitOutput {
    match index % 4 {
        0 => ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
            serde_json::json!({"property": true}),
        )),
        1 => ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::failure(
            crate::ToolFailure::runtime(
                crate::ToolFailureClass::External,
                "property_failure",
                "generated failure",
            ),
        )),
        2 => ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::cancelled(
            crate::ToolCancellation::runtime("generated cancellation"),
        )),
        _ => ProcessAwaitOutput::Abandoned {
            evidence: Box::new(crate::AbandonEvidence {
                writer: crate::AbandonWriter::Producer,
                owner: None,
                epoch_ms: 1,
            }),
            control: None,
        },
    }
}

async fn assert_rejected_write_is_noop(
    registry: &Arc<dyn ProcessRegistry>,
    id: &ProcessId,
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

async fn registry_snapshot(
    registry: &Arc<dyn ProcessRegistry>,
    id: &ProcessId,
) -> serde_json::Value {
    let record = registry.get_process(id).await.ok().flatten();
    let events = registry.full_event_window(id, 0).await.unwrap_or_default();
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
            .and_then(|process| match &process.lifecycle {
                ProcessLifecycle::Live { base, .. } => Some(base.as_ref().clone()),
                _ => None,
            })
        else {
            continue;
        };
        let stored = registry
            .get_process(&id)
            .await
            .map_err(|error| format!("live modeled process `{id}` became unavailable: {error}"))?
            .ok_or_else(|| format!("live modeled process `{id}` disappeared"))?;
        let events = registry
            .full_event_window(&id, 0)
            .await
            .map_err(|error| error.to_string())?;
        let folded = fold_process_record(base, &events).map_err(|error| error.to_string())?;
        if folded != stored {
            return Err(format!(
                "stored record for `{id}` differs from folding its retained event history"
            ));
        }
    }
    Ok(())
}

async fn assert_replay_key_idempotency(
    registry: &Arc<dyn ProcessRegistry>,
) -> Result<(), TestCaseError> {
    let id = registry
        .register_process(registration("law-replay-key", None))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .id;
    let request =
        ProcessEventAppendRequest::new("property.signal", serde_json::json!({"value": 1}))
            .with_replay_key("law-replay-key:stable");
    let first = registry
        .append_event(&id, request.clone())
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let replay = registry
        .append_event(&id, request)
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
            .full_event_window(&id, 0)
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?
            .len(),
        1
    );
    let conflict = registry
        .append_event(
            &id,
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
            .full_event_window(&id, 0)
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?
            .len(),
        1
    );
    Ok(())
}

/// The registry keeps the execution-started fact consistent; the engine alone
/// decides whether a start may run (ADR 0110). The same execution is
/// idempotent, and a successor execution takes exactly the next attempt.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn assert_attempt_monotonicity(
    registry: &Arc<dyn ProcessRegistry>,
) -> Result<(), TestCaseError> {
    let executed = registry
        .register_process(registration("law-attempt-monotonicity", None))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .id;
    for attempt in 1..=2 {
        let authority = invocation_authority(&executed, attempt as u8, attempt);
        let started = authority.invocation_started().expect("bound invocation");
        let outcome = registry
            .record_first_started_with_authority(&executed, started, &authority)
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        prop_assert!(matches!(outcome, ProcessStartOutcome::Started(_)));
    }
    let current = invocation_authority(&executed, 2, 2);
    let repeated = registry
        .record_first_started_with_authority(
            &executed,
            current.invocation_started().expect("bound invocation"),
            &current,
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        matches!(repeated, ProcessStartOutcome::AlreadyApplied(_)),
        "Attempt monotonicity: the current execution's start was not idempotent"
    );
    let skipped = invocation_authority(&executed, 3, 4);
    prop_assert!(
        registry
            .record_first_started_with_authority(
                &executed,
                skipped.invocation_started().expect("bound invocation"),
                &skipped,
            )
            .await
            .is_err(),
        "Attempt monotonicity: a start that skipped an attempt was accepted"
    );

    Ok(())
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn assert_stale_authority_non_mutation(
    registry: &Arc<dyn ProcessRegistry>,
) -> Result<(), TestCaseError> {
    let id = registry
        .register_process(registration("law-stale-authority", None))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .id;
    let current = invocation_authority(&id, 1, 1);
    registry
        .record_first_started_with_authority(
            &id,
            current.invocation_started().expect("bound invocation"),
            &current,
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let successor = invocation_authority(&id, 2, 2);
    registry
        .record_first_started_with_authority(
            &id,
            successor.invocation_started().expect("bound invocation"),
            &successor,
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let before = registry_snapshot(registry, &id).await;
    let stale = registry
        .append_event_with_authority(
            &id,
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
        registry_snapshot(registry, &id).await,
        before,
        "Stale-authority non-mutation: superseded attempt changed durable state"
    );
    Ok(())
}

async fn assert_enqueued_wake_high_water_safety(
    runtime: &Arc<dyn RuntimeStore>,
) -> Result<(), TestCaseError> {
    let session = SessionId::from("law-high-water");
    let process = crate::ProcessId::fixture("law-high-water-process");
    // Removal is intentionally not required to be contiguous: a host cancel withdraws any open
    // row. The law is that withdrawing a later sequence never removes an already-enqueued lower
    // row.
    let earlier = runtime
        .enqueue_queued_work(process_wake_batch_draft(runtime_wake_for(
            &session, &process, 1,
        )))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let later = runtime
        .enqueue_queued_work(process_wake_batch_draft(runtime_wake_for(
            &session, &process, 2,
        )))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    runtime
        .cancel_queued_work_batch(&session, &later.batch_id)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| {
            TestCaseError::fail("Enqueued-wake high-water safety: later wake was not cancellable")
        })?;
    let after_later = runtime
        .list_queued_work(&session)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(
        after_later
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![earlier.batch_id.as_str()],
        "Enqueued-wake high-water safety: cancelling sequence 2 removed or disturbed live sequence 1"
    );

    // The cancelled tombstone answers the redelivery.
    let answered = runtime
        .enqueue_queued_work_with_outcome(process_wake_batch_draft(runtime_wake_for(
            &session, &process, 2,
        )))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        matches!(
            &answered,
            crate::QueuedWorkEnqueueOutcome::Existing(batch)
                if batch.batch_id == later.batch_id && batch.terminal.is_some()
        ),
        "Enqueued-wake high-water safety: the redelivery must answer the cancelled tombstone: \
         {answered:?}"
    );

    // A retry whose receiver row is still live is idempotent: the live row is the durable
    // evidence that this exact semantic source was accepted. The retry's delivery metadata may
    // differ; its process fact may not (ADR 0101 §8).
    let mut changed = runtime_wake_for(&session, &process, 1);
    changed.input = "a different process fact under the same sequence".to_string();
    let conflict = runtime
        .enqueue_queued_work_with_outcome(process_wake_batch_draft(changed))
        .await;
    prop_assert!(
        matches!(
            &conflict,
            Err(StoreError::QueuedWorkSourceKeyConflict { existing_batch_id, .. })
                if *existing_batch_id == earlier.batch_id
        ),
        "Enqueued-wake source key: a changed process fact must be a typed conflict: \
         {conflict:?}"
    );
    let mut rewound = runtime_wake_for(&session, &process, 1);
    rewound.created_at_ms = 2;
    let retry = runtime
        .enqueue_queued_work_with_outcome(process_wake_batch_draft(rewound))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let crate::QueuedWorkEnqueueOutcome::Existing(retried_batch) = retry else {
        return Err(TestCaseError::fail(
            "Enqueued-wake source key: live-row retry was not idempotent",
        ));
    };
    prop_assert_eq!(
        retried_batch.batch_id,
        earlier.batch_id.clone(),
        "Enqueued-wake source key: the live-row retry answered another batch"
    );
    let after_live_retry = runtime
        .list_queued_work(&session)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert_eq!(
        after_live_retry
            .iter()
            .map(|batch| batch.batch_id.as_str())
            .collect::<Vec<_>>(),
        vec![earlier.batch_id.as_str()],
        "Enqueued-wake source key: live-row absorption changed pending receiver work"
    );

    let head = runtime
        .list_queued_work(&session)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .into_iter()
        .next()
        .ok_or_else(|| {
            TestCaseError::fail(
                "Enqueued-wake high-water safety: lower wake did not remain admissible",
            )
        })?;
    // The wake leaves through a terminal transition: the host cancel raises
    // the receiver floor exactly as a settled claim does (FIG-3545).
    runtime
        .cancel_queued_work_batch(&session, &head.batch_id)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("Enqueued-wake high-water safety: head not open"))?;
    prop_assert!(
        runtime
            .list_queued_work(&session)
            .await
            .map_err(|error| TestCaseError::fail(error.to_string()))?
            .is_empty(),
        "Enqueued-wake high-water safety: sequence 1 did not consume exactly once"
    );
    Ok(())
}

async fn assert_prune_reregister_wake_names_the_new_run(
    handles: &StoreContractHandles,
) -> Result<(), TestCaseError> {
    let session = SessionId::from("law-prune-wake");
    let start_key = crate::StartKey::for_host("law-prune-reregister-wake-process");
    let process = handles
        .registry
        .register_process(
            registration("law-prune-reregister-wake-process", Some(session.clone()))
                .with_start_key(Some(start_key.clone())),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .id;
    let original = handles
        .registry
        .append_event(
            &process,
            ProcessEventAppendRequest::new(
                "property.wake",
                serde_json::json!({"wake_input": "old incarnation"}),
            )
            .with_replay_key("law:prune-reregister-wake:old"),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .wake_delivery
        .ok_or_else(|| TestCaseError::fail("old incarnation did not materialize a wake"))?;
    handles
        .runtime
        .enqueue_queued_work(process_wake_batch_draft(original))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;

    let head = handles
        .runtime
        .list_queued_work(&session)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .into_iter()
        .next()
        .ok_or_else(|| TestCaseError::fail("old-incarnation wake was not admissible"))?;
    handles
        .runtime
        .cancel_queued_work_batch(&session, &head.batch_id)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .ok_or_else(|| TestCaseError::fail("old-incarnation wake was not open"))?;

    handles
        .registry
        .complete_process(
            &process,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!("old incarnation done"),
            )),
            ProcessCompletionAuthority::workflow_key(&process),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let (_, terminal_cursor) = handles
        .registry
        .processes_changed_since(ProcessChangeCursor::initial(), 1_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    handles
        .registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::UpTo(terminal_cursor))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    // The same start key after prune starts a new run under a new id: the
    // pruned run's wakes can never be confused with the new run's (ADR 0107).
    let restarted = handles
        .registry
        .register_process(
            registration("law-prune-reregister-wake-process", Some(session.clone()))
                .with_start_key(Some(start_key)),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .id;
    prop_assert_ne!(
        &restarted,
        &process,
        "prune/restart wake: the restarted run reused the pruned run's id"
    );
    let replacement = handles
        .registry
        .append_event(
            &restarted,
            ProcessEventAppendRequest::new(
                "property.wake",
                serde_json::json!({"wake_input": "new incarnation"}),
            )
            .with_replay_key("law:prune-reregister-wake:new"),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .wake_delivery
        .ok_or_else(|| TestCaseError::fail("new incarnation did not materialize a wake"))?;
    prop_assert_eq!(
        &replacement.process_id,
        &restarted,
        "prune/restart wake: the new run's wake names the new run"
    );
    let receipt = handles
        .runtime
        .enqueue_queued_work(process_wake_batch_draft(replacement))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        receipt.enqueue_seq > 0,
        "prune/restart wake was suppressed instead of enqueued"
    );
    Ok(())
}

async fn assert_prune_tombstone_watermark_safety(
    registry: &Arc<dyn ProcessRegistry>,
) -> Result<(), TestCaseError> {
    let live_id = registry
        .register_process(registration("law-prune-live-must-survive", None))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .id;
    registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        matches!(registry.get_process(&live_id).await, Ok(Some(_))),
        "Prune/tombstone/watermark safety: live process was pruned"
    );
    let eligible_id = registry
        .register_process(registration("law-prune-watermark-eligible", None))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .id;
    registry
        .complete_process(
            &eligible_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            ProcessCompletionAuthority::workflow_key(&eligible_id),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let id = registry
        .register_process(registration("law-prune-watermark", None))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .id;
    let (_, before_terminal) = registry
        .processes_changed_since(ProcessChangeCursor::initial(), 1_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    registry
        .complete_process(
            &id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            ProcessCompletionAuthority::workflow_key(&id),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    registry
        .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::UpTo(before_terminal))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    prop_assert!(
        matches!(registry.get_process(&id).await, Ok(Some(_))),
        "Prune/tombstone/watermark safety: subject terminal was pruned before projection watermark passed it"
    );
    prop_assert!(
        matches!(
            registry.get_process(&eligible_id).await,
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
    let tombstone = registry.get_process(&id).await;
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
            registry.get_process(&id).await,
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
        matches!(registry.get_process(&id).await, Ok(None)),
        "Prune/tombstone/watermark safety: projected subject tombstone was not compacted"
    );
    prop_assert!(
        matches!(registry.get_process(&live_id).await, Ok(Some(_))),
        "Prune/tombstone/watermark safety: live process did not survive the full prune lifecycle"
    );
    Ok(())
}

/// A run restarted under the same start key after its predecessor was pruned
/// is a new process: a new id, a fresh event log, and a record its own
/// baseline folds to, while the pruned run stays a tombstone that compacts
/// independently of it (ADR 0107).
async fn assert_prune_reregister_registry_state_is_fresh(
    registry: &Arc<dyn ProcessRegistry>,
) -> Result<(), TestCaseError> {
    let start_key = crate::StartKey::for_host("law-prune-reregister");
    let id = registry
        .register_process(
            registration(
                "law-prune-reregister",
                Some(SessionId::from("law-prune-reregister-old")),
            )
            .with_start_key(Some(start_key.clone())),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .id;
    registry
        .append_event(
            &id,
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
            &id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::json!({"identity": "old"}),
            )),
            ProcessCompletionAuthority::workflow_key(&id),
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
            registry.get_process(&id).await,
            Err(crate::PluginError::ProcessNoLongerRetained { .. })
        ),
        "Prune/re-register registry state: prune did not leave a tombstone"
    );
    let (_, deletion_cursor) = registry
        .processes_changed_since(terminal_cursor, 1_000)
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;

    let fresh_base = registry
        .register_process(
            registration(
                "law-prune-reregister",
                Some(SessionId::from("law-prune-reregister-new")),
            )
            .with_start_key(Some(start_key)),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?;
    let fresh_id = fresh_base.id.clone();
    prop_assert_ne!(
        &fresh_id,
        &id,
        "Prune/restart registry state: the restart reused the pruned run's id"
    );
    prop_assert!(
        matches!(
            registry.get_process(&id).await,
            Err(crate::PluginError::ProcessNoLongerRetained { .. })
        ),
        "Prune/restart registry state: the restart revived the pruned run"
    );
    let live = registry
        .get_process(&fresh_id)
        .await
        .map_err(|error| TestCaseError::fail(format!(
            "Prune/re-register registry state: fresh live record did not shadow stale tombstone: {error}"
        )))?
        .ok_or_else(|| {
            TestCaseError::fail("Prune/re-register registry state: fresh live record was absent")
        })?;
    let fresh_events = registry
        .full_event_window(&fresh_id, 0)
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
        matches!(registry.get_process(&fresh_id).await, Ok(Some(_))),
        "Prune/restart registry state: compacting the stale tombstone removed the restarted run"
    );
    Ok(())
}

fn runtime_wake(process_id: &ProcessId, sequence: u64) -> ProcessWakeDelivery {
    runtime_wake_for(
        &SessionId::from("prop-runtime-session"),
        process_id,
        sequence,
    )
}
fn runtime_wake_for(
    session_id: &SessionId,
    process_id: &ProcessId,
    sequence: u64,
) -> ProcessWakeDelivery {
    ProcessWakeDelivery {
        version: crate::FleetFormat::current().writer_version(lash_core::surface_format!(
            PROCESS_WAKE_DELIVERY_FORMAT_VERSION
        )),
        target_session_id: session_id.clone(),
        process_id: process_id.clone(),
        sequence,
        event_type: "property.wake".to_string(),
        process_caused_by: None,
        authority: crate::QueuedWorkAuthority::default(),
        input: format!("wake-{sequence}"),
        created_at_ms: 1,
        trace_cause: Default::default(),
    }
}

async fn consume_wake(
    runtime: &dyn RuntimeStore,
    process_id: &ProcessId,
    sequence: u64,
) -> Result<bool, String> {
    let session = SessionId::from("prop-runtime-session");
    let queued = runtime
        .list_queued_work(&session)
        .await
        .map_err(|error| error.to_string())?;
    let Some(batch) = queued.iter().find(|batch| matches!(
        &batch.payload, QueuedWorkPayload::ProcessWake { wake } if wake.process_id == process_id && wake.sequence == sequence
    )) else { return Ok(false); };
    // A wake leaves the lane through a terminal transition. The one a store
    // law can drive without a session actor is the host cancel, which raises
    // the same receiver floor a settled claim does (FIG-3545).
    runtime
        .cancel_queued_work_batch(&session, &batch.batch_id)
        .await
        .map(|cancelled| cancelled.is_some())
        .map_err(|error| error.to_string())
}
