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
    apply_process_event_projection, fold_process_record,
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
const GENERATED_PREFIX_OPS: usize = 8;
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
    Register { process: u8 },
    FirstStart { process: u8, owner: u8, attempt: u8 },
    EnterWait { process: u8, stale: bool },
    ClearWait { process: u8, stale: bool },
    SetExternalRef { process: u8, value: u8 },
    CancelRequest { process: u8, requester: u8 },
    Terminal { process: u8, disposition: u8 },
    AddObserver { process: u8, session: u8 },
    RemoveObserver { process: u8, session: u8 },
    Prune { watermark: bool },
    CompactTombstones { caught_up: bool },
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
    observers: BTreeSet<SessionId>,
    lifecycle_replay_keys: BTreeSet<String>,
    current_authority: Option<ProcessExecutionWriteAuthority>,
    superseded_authorities: Vec<ProcessExecutionWriteAuthority>,
}

#[derive(Clone, Debug, Default)]
struct ReferenceModel {
    /// The id the registrar minted for each generated process slot's current
    /// run. A slot re-registered after its run was pruned names a new run.
    slot_ids: BTreeMap<u8, ProcessId>,
    processes: BTreeMap<ProcessId, ModelProcess>,
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
fn registration(label: &str) -> ProcessRegistration {
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

fn wait_state(_process_id: &ProcessId) -> WaitState {
    WaitState {
        since_ms: 1,
        kind: WaitKind::Call {
            call_id: lash_sansio::ToolCallId::fixture("store-call"),
            tool_id: lash_sansio::ToolId::new("store_call"),
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
        StoreContractOp::Register { process } => {
            let slot = *process % PROCESS_COUNT;
            let result = handles
                .registry
                .register_process(
                    registration(&format!("prop-process-{slot}"))
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
                .compact_process_tombstones(u64::MAX, watermark)
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
        .register_process(registration("law-replay-key"))
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .id;
    let request = call_wait_event(
        &id,
        "store-contract",
        "fixture",
        serde_json::json!({"value": 1}),
    )
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
            call_wait_event(
                &id,
                "store-contract",
                "fixture",
                serde_json::json!({"value": 2}),
            )
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
        .register_process(registration("law-attempt-monotonicity"))
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
        .register_process(registration("law-stale-authority"))
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
            call_wait_event(
                &id,
                "store-contract",
                "fixture",
                serde_json::json!({"stale": true}),
            )
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

async fn assert_prune_tombstone_watermark_safety(
    registry: &Arc<dyn ProcessRegistry>,
) -> Result<(), TestCaseError> {
    let live_id = registry
        .register_process(registration("law-prune-live-must-survive"))
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
        .register_process(registration("law-prune-watermark-eligible"))
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
        .register_process(registration("law-prune-watermark"))
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
        .compact_process_tombstones(u64::MAX, ProjectionWatermark::UpTo(terminal_cursor))
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
        .compact_process_tombstones(u64::MAX, ProjectionWatermark::UpTo(deletion_cursor))
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
            registration("law-prune-reregister").with_start_key(Some(start_key.clone())),
        )
        .await
        .map_err(|error| TestCaseError::fail(error.to_string()))?
        .id;
    registry
        .append_event(
            &id,
            call_wait_event(
                &id,
                "store-contract",
                "fixture",
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
        .register_process(registration("law-prune-reregister").with_start_key(Some(start_key)))
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
        .compact_process_tombstones(u64::MAX, ProjectionWatermark::UpTo(deletion_cursor))
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
