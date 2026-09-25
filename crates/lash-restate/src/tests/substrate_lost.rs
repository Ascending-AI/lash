//! Lash never restarts started work from scratch on Restate (FIG-3588).
//!
//! Every law here drives the real `LashProcessWorkflow/run` handler through
//! the Restate protocol, with Restate's own identity rule: a workflow's
//! invocation id is a function of its key, so the harness gives a retry and a
//! fresh invocation of one key the same id. A retry replays the journal the
//! runtime acknowledged ([`encode_journal_retry`]); a fresh invocation — the
//! journal lost to an endpoint crash or lost journal storage — starts from an
//! empty one.
//!
//! The admission order under test: a read-only verdict step that journals a
//! nonce, then the start marker written set-if-absent with that nonce, then
//! the segment's first effect. The laws:
//!
//! - (a) a crash between the two steps retries without a false refusal;
//! - (b) a journal lost before the marker commits admits a fresh run, and no
//!   effect had run;
//! - (c) a journal lost after the marker, before the first effect, is
//!   `SubstrateLost` with zero effects (the accepted double-fault direction);
//! - (d) a journal lost after effects is `SubstrateLost` with zero
//!   re-dispatch.

use super::*;
use lashlang::testing::ast_builders as b;

/// `main` calls the counting tool once per segment: with a one-effect budget,
/// segment 0 runs `first`, segment 1 runs `second`.
async fn two_segment_tool_registration(process_id: &ProcessId) -> ProcessRegistration {
    let call = |line: &str| {
        b::module_call(
            &["tools"],
            "recovery_count",
            vec![b::record(vec![("line", b::string(line))])],
        )
    };
    let module = b::module(
        vec![b::process(
            "main",
            Vec::new(),
            b::block(vec![
                b::assign("first", call("first")),
                b::assign("second", call("second")),
                b::finish(b::string("done")),
            ]),
        )],
        Vec::new(),
    );
    let contract = CountingProcessTool::definition().contract();
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_module_operation_contract(
            ["tools"],
            "Tools",
            "recovery_count",
            "tool:recovery_count",
            &lashlang::OperationContract::new(
                contract.input_schema.canonical().clone(),
                contract.output_schema.canonical().clone(),
            ),
        )
        .expect("link the counting tool operation");
    let linked = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(resources, lashlang::LashlangAbilities::default()),
    )
    .expect("link the two-segment process");
    lashlang::LashlangArtifacts::publish_module_artifact(
        &recovery_artifact_store(),
        &lash_core::ArtifactOwner::host("restate-substrate-lost"),
        &linked.artifact,
    )
    .await
    .expect("store the two-segment artifact");
    ProcessRegistration::new(
        process_id.clone(),
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked.artifact.module_ref().clone(),
            process_ref: linked
                .artifact
                .process_ref("main")
                .expect("main process ref")
                .clone(),
            host_requirements_ref: linked.artifact.host_requirements_ref().clone(),
            process_name: "main".to_string(),
            args: serde_json::Map::new(),
        }),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::host(),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(persist_recovery_env_ref().await))
}

fn substrate_lost(owner: lash_core::LeaseOwnerIdentity) -> impl Fn(&ProcessAwaitOutput) -> bool {
    move |output| {
        matches!(
            output,
            ProcessAwaitOutput::Abandoned { evidence, control: None }
                if evidence.writer
                    == lash_core::AbandonWriter::ResumeRefused {
                        reason: lash_core::ProcessResumeRefusal::SubstrateLost,
                    }
                    && evidence.owner.as_ref() == Some(&owner)
        )
    }
}

const ROOT_EXECUTION: &str = "root-nonce";

fn segment_input(
    registration: &ProcessRegistration,
    segment_ordinal: u64,
) -> RestateProcessWorkflowInput {
    RestateProcessWorkflowInput {
        registration: registration.clone(),
        execution_context: ProcessExecutionContext::default(),
        segment_ordinal,
        journal_version: RESTATE_PROCESS_JOURNAL_VERSION,
    }
}

/// Counts the segment runs it is driven for — the only path any effect of
/// the segment can take — and settles each successfully, or crashes after the
/// run's effect when told to.
#[derive(Default)]
struct EffectRunner {
    runs: AtomicUsize,
    crash_after_effect: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl RestateProcessRunner for EffectRunner {
    fn replay_key_grammar(&self, _registration: &ProcessRegistration) -> Option<u32> {
        None
    }

    async fn run_process_segment(
        &self,
        _started: &SegmentStarted,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if self.crash_after_effect.load(Ordering::SeqCst) {
            return Err(PluginError::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::RuntimeStore,
                "injected crash after the segment's effect",
            )));
        }
        Ok(lash_core::ProcessRunOutcome::Terminal {
            output: Box::new(process_success(serde_json::json!("ran"))),
        })
    }
}

/// A process whose root execution started and handed segment 1 over, served
/// by an endpoint whose runner is `runner`.
struct HandedOverSegment {
    registration: ProcessRegistration,
    registry: Arc<dyn ProcessRegistry>,
    continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    root_start: lash_core::ProcessStarted,
    runner: Arc<EffectRunner>,
    endpoint: Endpoint,
}

impl HandedOverSegment {
    async fn new(process_id: &str) -> Self {
        let (registry, continuations) = process_stores();
        let registration = rerunnable_registration(process_id);
        registry
            .register_process(registration.clone())
            .await
            .expect("register the handed-over process");
        let (authority, root_start) =
            invocation_started(&ProcessId::from(process_id), ROOT_EXECUTION, 1);
        registry
            .record_first_started_with_authority(
                &ProcessId::from(process_id),
                root_start.clone(),
                &authority,
            )
            .await
            .expect("record the root execution's start");
        continuations
            .put_segment_handover(
                &ProcessId::from(process_id),
                lash_core::PersistedSegmentHandover {
                    writer: String::new(),
                    segment_ordinal: 1,
                    handover: lash_core::SegmentHandover {
                        reason: lash_core::BoundaryReason::JournalBudget,
                        program_hash: "program-v1".to_string(),
                        engine_state: vec![1],
                    },
                },
            )
            .await
            .expect("hand segment 1 over");
        let runner = Arc::new(EffectRunner::default());
        let endpoint = Endpoint::builder()
            .bind(
                LashProcessWorkflowImpl::new_for_test(
                    Arc::clone(&runner),
                    Arc::clone(&registry),
                    Arc::clone(&continuations),
                )
                .serve(),
            )
            .build();
        Self {
            registration,
            registry,
            continuations,
            root_start,
            runner,
            endpoint,
        }
    }

    fn key(&self) -> String {
        process_segment_workflow_key(&self.registration.id, 1)
    }

    fn input(&self) -> RestateProcessWorkflowInput {
        segment_input(&self.registration, 1)
    }

    /// A fresh invocation of segment 1: the key's id with an empty journal.
    async fn invoke_fresh(&self, complete_runs: bool) -> bytes::Bytes {
        invoke_process_workflow_endpoint(
            &self.endpoint,
            "run",
            &self.key(),
            &self.input(),
            complete_runs,
        )
        .await
        .unwrap_or_default()
    }

    /// Restate's retry of an earlier try, whose first `journaled` commands
    /// the runtime acknowledged.
    async fn retry(&self, prior: &[u8], journaled: usize, complete_runs: bool) -> bytes::Bytes {
        let body = encode_journal_retry(&self.key(), &self.input(), prior, journaled)
            .expect("encode the acknowledged journal");
        invoke_process_workflow_body(&self.endpoint, "run", body, complete_runs)
            .await
            .unwrap_or_default()
    }

    async fn marker(&self) -> Option<lash_core::SegmentStartMarker> {
        self.continuations
            .segment_start(&lash_core::ProcessSegmentKey::new(
                self.registration.id.clone(),
                1,
            ))
            .await
            .expect("read the segment's start marker")
    }

    async fn outcome(&self) -> Option<ProcessAwaitOutput> {
        self.registry
            .get_process(&self.registration.id)
            .await
            .expect("read the process")
            .expect("the process exists")
            .outcome
    }

    fn runs(&self) -> usize {
        self.runner.runs.load(Ordering::SeqCst)
    }
}

/// The verdict step's proposal, before the runtime acknowledged it.
fn proposed_runs(output: &[u8]) -> usize {
    restate_message_types(output)
        .unwrap_or_default()
        .into_iter()
        .filter(|message_type| *message_type == 0x0005)
        .count()
}

/// Law (b): a journal lost before the marker committed leaves nothing
/// behind, so a fresh run of the segment is admitted — and no effect had run.
#[tokio::test]
pub(super) async fn law_b_a_journal_lost_before_the_marker_admits_a_fresh_run() {
    let segment = HandedOverSegment::new("admission-law-b").await;
    let lost = segment.invoke_fresh(false).await;
    assert_eq!(
        proposed_runs(&lost),
        1,
        "the try proposed its verdict: {lost:?}"
    );
    assert!(
        segment.marker().await.is_none(),
        "the verdict writes nothing"
    );
    assert_eq!(segment.runs(), 0, "nothing ran before the marker");

    segment.invoke_fresh(true).await;
    assert_eq!(segment.runs(), 1, "the fresh invocation runs the segment");
    assert!(segment.marker().await.is_some());
    assert_eq!(
        segment.outcome().await,
        Some(process_success(serde_json::json!("ran")))
    );
}

/// Law (a): a crash between the verdict and the marker — here after the
/// start step wrote the marker but before its completion was journaled — is
/// retried by Restate over the journaled verdict. The retry's start step finds
/// its own nonce and proceeds; nothing is refused.
#[tokio::test]
pub(super) async fn law_a_a_crash_between_verdict_and_marker_retries_without_refusal() {
    let segment = HandedOverSegment::new("admission-law-a").await;
    let first = segment.invoke_fresh(false).await;
    // The runtime acknowledged the verdict; the start step then wrote the
    // marker and the endpoint died before its completion was journaled.
    let crashed = segment.retry(&first, 1, false).await;
    assert_eq!(
        proposed_runs(&crashed),
        1,
        "the start step proposed: {crashed:?}"
    );
    let marker = segment
        .marker()
        .await
        .expect("the start step wrote its marker");
    assert_eq!(segment.runs(), 0);

    segment.retry(&first, 1, true).await;
    assert_eq!(
        segment.marker().await,
        Some(marker),
        "the retry recognised its own marker"
    );
    assert_eq!(segment.runs(), 1, "the retried segment runs once");
    assert_eq!(
        segment.outcome().await,
        Some(process_success(serde_json::json!("ran")))
    );
}

/// Law (c): a journal lost after the marker committed but before the first
/// effect cannot be told from a lost journal with effects, so the process
/// ends `SubstrateLost` — with zero effects. This is the accepted direction
/// of the double fault: a false Abandoned, never a duplicate effect.
#[tokio::test]
pub(super) async fn law_c_a_journal_lost_after_the_marker_is_substrate_lost_with_no_effect() {
    let segment = HandedOverSegment::new("admission-law-c").await;
    let first = segment.invoke_fresh(false).await;
    segment.retry(&first, 1, false).await;
    assert!(segment.marker().await.is_some());

    segment.invoke_fresh(true).await;
    assert_eq!(segment.runs(), 0, "no effect ever ran");
    let outcome = segment.outcome().await.expect("the refusal is stored");
    assert!(
        substrate_lost(segment.root_start.owner.clone())(&outcome),
        "got {outcome:?}"
    );
}

/// Law (d): a journal lost after the segment's effects is `SubstrateLost`,
/// and the effects are not dispatched again.
#[tokio::test]
pub(super) async fn law_d_a_journal_lost_after_effects_is_substrate_lost_with_no_redispatch() {
    let segment = HandedOverSegment::new("admission-law-d").await;
    segment
        .runner
        .crash_after_effect
        .store(true, Ordering::SeqCst);
    let crashed = segment.invoke_fresh(true).await;
    assert!(
        restate_error_message(&crashed).is_some(),
        "the crash is a retryable failure: {crashed:?}"
    );
    assert_eq!(segment.runs(), 1, "the segment's effect ran once");

    segment
        .runner
        .crash_after_effect
        .store(false, Ordering::SeqCst);
    segment.invoke_fresh(true).await;
    assert_eq!(
        segment.runs(),
        1,
        "the lost journal's effect is never re-dispatched"
    );
    let outcome = segment.outcome().await.expect("the refusal is stored");
    assert!(
        substrate_lost(segment.root_start.owner.clone())(&outcome),
        "got {outcome:?}"
    );
}

/// Law (d) with a real tool: segment 1's tool call ran and was journaled,
/// then its boundary failed on a store fault and its journal was lost before
/// Restate retried it. The fresh invocation ends `SubstrateLost`; the tool
/// runs once.
#[tokio::test]
pub(super) async fn law_d_a_real_tool_call_is_never_executed_twice() {
    let process_id = ProcessId::from("admission-law-d-tool");
    let executions = Arc::new(AtomicUsize::new(0));
    let stores = memory_process_stores().await;
    let registry: Arc<dyn ProcessRegistry> = stores.registry.clone();
    let continuations = Arc::clone(&stores.continuations);
    let worker = recovery_worker_with_plugins(
        Arc::clone(&registry),
        memory_session_store_factory().await,
        vec![counting_tool_plugin(Arc::clone(&executions))],
    )
    .await;
    let registration = two_segment_tool_registration(&process_id).await;
    registry
        .register_process(registration.clone())
        .await
        .expect("register the two-segment process");

    // Segment 0 runs its tool and hands segment 1 over.
    let workflow = LashProcessWorkflowImpl::new_for_test(
        Arc::new(RestateCoreProcessRunner::new(worker.clone())),
        Arc::clone(&registry),
        Arc::clone(&continuations),
    )
    .with_segment_effect_budget_selector(|_| 1);
    let context = Arc::new(ReplayableRecordingContext::default());
    let controller = RestateRuntimeEffectController::with_options_for_test(
        Arc::clone(&context),
        RestateEffectControllerOptions::default().segment_effect_budget(1),
    );
    let outcome = workflow
        .run_registration_for_test(
            registration.clone(),
            ProcessExecutionContext::default().with_execution_write_authority(
                lash_core::ProcessExecutionWriteAuthority::invocation(&process_id, ROOT_EXECUTION),
            ),
            controller
                .process_scope_for_test(durable_admission(&ExecutionScope::process(&process_id)))
                .expect("segment 0 scope"),
            0,
            None,
        )
        .await
        .expect("run segment 0");
    let lash_core::ProcessRunOutcome::SegmentBoundary(boundary) = outcome else {
        panic!("segment 0 crosses its one-effect boundary, got {outcome:?}");
    };
    continuations
        .put_segment_handover(
            &process_id,
            lash_core::PersistedSegmentHandover {
                writer: String::new(),
                segment_ordinal: 1,
                handover: boundary,
            },
        )
        .await
        .expect("hand segment 1 over");
    assert_eq!(executions.load(Ordering::SeqCst), 1);

    // Segment 1 runs through the real handler; its tool call runs and is
    // journaled, then its boundary's store write fails.
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(RestateCoreProcessRunner::new(worker)),
                Arc::clone(&registry),
                Arc::clone(&continuations),
            )
            .with_segment_effect_budget_selector(|_| 1)
            .serve(),
        )
        .build();
    stores
        .registry
        .fail_next_external_ref_write(PluginError::Runtime(lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::RuntimeStore,
            "injected store fault at segment 1's boundary",
        )));
    let key = process_segment_workflow_key(&process_id, 1);
    let crashed = invoke_process_workflow_endpoint(
        &endpoint,
        "run",
        &key,
        &segment_input(&registration, 1),
        true,
    )
    .await
    .unwrap_or_default();
    assert!(
        restate_error_message(&crashed).is_some(),
        "the boundary's store fault is retryable: {crashed:?}"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 2, "segment 1's tool ran");

    // Its journal is lost; a fresh invocation of the key arrives.
    let _ = invoke_process_workflow_endpoint(
        &endpoint,
        "run",
        &key,
        &segment_input(&registration, 1),
        true,
    )
    .await;
    assert_eq!(
        executions.load(Ordering::SeqCst),
        2,
        "segment 1's recorded tool call is never executed again"
    );
    let record = registry
        .get_process(&process_id)
        .await
        .expect("read the refused process")
        .expect("the process exists");
    let started = record.first_started.as_deref().cloned().expect("started");
    let outcome = record.outcome.expect("the refusal is stored");
    assert!(substrate_lost(started.owner)(&outcome), "got {outcome:?}");
}

/// Admission writes a fresh process's start record, so that record must be
/// the one its engine requires (the FIG-3588 regression). A lashlang process
/// submitted fresh to the real handler is admitted, and its start record names
/// the lashlang replay-key grammar: its body runs its first tool call and the
/// process stays Running across its first boundary. An unstamped record is
/// refused by the lashlang engine at the grammar cutover before any body runs,
/// so the process would end Failed with no tool call.
#[tokio::test]
pub(super) async fn an_admitted_lashlang_process_runs_its_body_and_is_running() {
    let process_id = ProcessId::from("admission-fresh-lashlang");
    let executions = Arc::new(AtomicUsize::new(0));
    let stores = memory_process_stores().await;
    let registry: Arc<dyn ProcessRegistry> = stores.registry.clone();
    let continuations = Arc::clone(&stores.continuations);
    let worker = recovery_worker_with_plugins(
        Arc::clone(&registry),
        memory_session_store_factory().await,
        vec![counting_tool_plugin(Arc::clone(&executions))],
    )
    .await;
    let registration = two_segment_tool_registration(&process_id).await;
    registry
        .register_process(registration.clone())
        .await
        .expect("register the process");
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(RestateCoreProcessRunner::new(worker)),
                Arc::clone(&registry),
                Arc::clone(&continuations),
            )
            .with_segment_effect_budget_selector(|_| 1)
            .serve(),
        )
        .build();

    let _ = invoke_process_workflow_endpoint(
        &endpoint,
        "run",
        &process_segment_workflow_key(&process_id, 0),
        &segment_input(&registration, 0),
        true,
    )
    .await;

    let record = registry
        .get_process(&process_id)
        .await
        .expect("read the admitted process")
        .expect("the process exists");
    let started = record
        .first_started
        .as_deref()
        .cloned()
        .expect("admission recorded the start");
    assert_eq!(
        started.replay_grammar,
        Some(lash_lashlang_runtime::LASHLANG_REPLAY_KEY_GRAMMAR_VERSION),
        "the admitted start record names the grammar the lashlang engine journals under"
    );
    assert!(
        record.outcome.is_none(),
        "an admitted fresh process is never refused before its body runs: {:?}",
        record.outcome
    );
    assert_eq!(
        record.status,
        lash_core::ProcessStatus::Running,
        "the admitted process is observable Running across its first boundary"
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "the admitted body ran its first tool call"
    );
}

/// A segment that already handed over completed: its successor carries the
/// process. A late or fresh invocation of it is ignored, never refused — a
/// false `SubstrateLost` there would abandon a healthy process.
#[tokio::test]
pub(super) async fn a_completed_segment_is_superseded_not_refused() {
    let segment = HandedOverSegment::new("admission-completed-segment").await;
    let first = segment.invoke_fresh(false).await;
    segment.retry(&first, 1, false).await;
    assert!(segment.marker().await.is_some(), "segment 1 started");
    segment
        .continuations
        .put_segment_handover(
            &segment.registration.id,
            lash_core::PersistedSegmentHandover {
                writer: String::new(),
                segment_ordinal: 2,
                handover: lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: "program-v1".to_string(),
                    engine_state: vec![2],
                },
            },
        )
        .await
        .expect("segment 1 handed over");

    let output = segment.invoke_fresh(true).await;
    assert_eq!(
        restate_output_json::<RestateProcessWorkflowOutput>(&output),
        Some(RestateProcessWorkflowOutput::SegmentChained {
            next_segment_ordinal: 2
        })
    );
    assert_eq!(segment.runs(), 0);
    assert_eq!(segment.outcome().await, None, "the process stays live");
}

/// Segment 0's marker is the process's `first_started`. Under Restate's
/// identity rule a retry and a fresh invocation of the root key share an id,
/// so the fence reads the marker, not the id: a started row is refused before
/// the runner is asked for anything, and a row that never started runs — the
/// sweep may still start rows that never started.
#[tokio::test]
pub(super) async fn root_segment_admits_only_rows_that_never_started() {
    let registry = process_registry();
    let started_id = ProcessId::from("admission-root-started");
    registry
        .register_process(rerunnable_registration(started_id.as_str()))
        .await
        .expect("register the started row");
    let (authority, started) = invocation_started(&started_id, ROOT_EXECUTION, 1);
    registry
        .record_first_started_with_authority(&started_id, started.clone(), &authority)
        .await
        .expect("record the lost execution's start");
    let runner = Arc::new(EffectRunner::default());
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::clone(&runner),
                Arc::clone(&registry),
                continuation_store(),
            )
            .serve(),
        )
        .build();
    let registration = rerunnable_registration(started_id.as_str());
    let _ = invoke_process_workflow_endpoint(
        &endpoint,
        "run",
        &process_segment_workflow_key(&started_id, 0),
        &segment_input(&registration, 0),
        true,
    )
    .await;
    assert_eq!(
        runner.runs.load(Ordering::SeqCst),
        0,
        "started work is never run from scratch"
    );
    let outcome = registry
        .get_process(&started_id)
        .await
        .expect("read the refused row")
        .and_then(|record| record.outcome)
        .expect("the refusal is a stored terminal");
    assert!(substrate_lost(started.owner)(&outcome), "got {outcome:?}");

    let fresh_id = ProcessId::from("admission-root-unstarted");
    let registration = rerunnable_registration(fresh_id.as_str());
    registry
        .register_process(registration.clone())
        .await
        .expect("register the unstarted row");
    invoke_process_workflow_endpoint(
        &endpoint,
        "run",
        &process_segment_workflow_key(&fresh_id, 0),
        &segment_input(&registration, 0),
        true,
    )
    .await
    .expect("the unstarted root segment runs");
    assert_eq!(
        runner.runs.load(Ordering::SeqCst),
        1,
        "a row that never started runs"
    );
    let record = registry
        .get_process(&fresh_id)
        .await
        .expect("read the run row")
        .expect("the row exists");
    assert_eq!(
        record.outcome,
        Some(process_success(serde_json::json!("ran")))
    );
    let root = record
        .first_started
        .expect("the start step recorded first_started");
    assert!(
        root.owner
            .engine_process_execution_id(&fresh_id)
            .is_some_and(|nonce| nonce != fresh_id.as_str()),
        "segment 0's execution is its journaled nonce, not the key-derived invocation id: {root:?}"
    );
}

/// Crosses the root segment's boundary once and hands nothing further over.
struct BoundaryRunner;

#[async_trait::async_trait]
impl RestateProcessRunner for BoundaryRunner {
    fn replay_key_grammar(&self, _registration: &ProcessRegistration) -> Option<u32> {
        None
    }

    async fn run_process_segment(
        &self,
        _started: &SegmentStarted,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        Ok(lash_core::ProcessRunOutcome::SegmentBoundary(
            lash_core::SegmentHandover {
                reason: lash_core::BoundaryReason::JournalBudget,
                program_hash: "program-v1".to_string(),
                engine_state: vec![1],
            },
        ))
    }
}

/// A store fault on the successor-reference write is retryable: the
/// invocation fails with Restate's retryable error, not a terminal output,
/// and Restate's retry — the same id over the journal it acknowledged —
/// writes the reference and hands over.
#[tokio::test]
pub(super) async fn a_successor_reference_store_fault_is_retried_by_restate() {
    let process_id = ProcessId::from("admission-ref-fault");
    let stores = memory_process_stores().await;
    let registry: Arc<dyn ProcessRegistry> = stores.registry.clone();
    let continuations = Arc::clone(&stores.continuations);
    let registration = rerunnable_registration(process_id.as_str());
    registry
        .register_process(registration.clone())
        .await
        .expect("register the handing-over row");
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(BoundaryRunner),
                Arc::clone(&registry),
                Arc::clone(&continuations),
            )
            .serve(),
        )
        .build();
    let input = segment_input(&registration, 0);
    stores
        .registry
        .fail_next_external_ref_write(PluginError::Runtime(lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::RuntimeStore,
            "injected transient reference write failure",
        )));
    let failed =
        invoke_process_workflow_endpoint(&endpoint, "run", process_id.as_str(), &input, true)
            .await
            .unwrap_or_default();
    assert!(
        restate_error_message(&failed).is_some(),
        "the fault is Restate's retryable error: {failed:?}"
    );
    assert!(
        restate_output_failure_message(&failed).is_none(),
        "the fault is not a terminal output: {failed:?}"
    );
    assert!(
        continuations
            .get_segment_handover(&process_id, 1)
            .await
            .expect("read the successor handover")
            .is_none(),
        "a boundary whose successor reference failed hands nothing over"
    );

    // The failed handover step's run command was never acknowledged: Restate
    // retries over the journal before it (FIG-3673).
    let commands = restate_recorded_commands(&failed).unwrap_or_default();
    assert_eq!(
        commands.last().map(|command| command.message_type),
        Some(RESTATE_RUN_COMMAND_MESSAGE_TYPE),
        "the attempt ends at its unacknowledged handover step"
    );
    let journaled = commands.len() - 1;
    let body = encode_journal_retry(process_id.as_str(), &input, &failed, journaled)
        .expect("encode Restate's retry");
    let _ = invoke_process_workflow_body(&endpoint, "run", body, true).await;
    assert!(
        continuations
            .get_segment_handover(&process_id, 1)
            .await
            .expect("read the successor handover")
            .is_some(),
        "the retried boundary hands over"
    );
    assert_eq!(
        registry
            .get_process(&process_id)
            .await
            .expect("read the handed-over row")
            .and_then(|record| record.external_ref)
            .map(|external| external.segment_ordinal()),
        Some(1),
        "the retry names the successor"
    );
}

/// The failure code a terminal record carries, if it ended Failed.
fn terminal_failure_code(record: &lash_core::ProcessRecord) -> Option<String> {
    let outcome = serde_json::to_value(record.outcome.as_ref()?).ok()?;
    fn find(value: &serde_json::Value) -> Option<String> {
        match value {
            serde_json::Value::Object(map) => map
                .get("code")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .or_else(|| map.values().find_map(find)),
            serde_json::Value::Array(items) => items.iter().find_map(find),
            _ => None,
        }
    }
    find(&outcome)
}

/// FIG-3809/FIG-3789: a handover write no retry can fix ends the process
/// Failed, typed `process_segment_handover_write`, and publishes the terminal
/// its awaiters wait on; the invocation never ends with the process Running.
#[tokio::test]
pub(super) async fn a_failed_handover_write_ends_the_process_failed_typed() {
    let process_id = ProcessId::from("segment-failure-handover-write");
    let stores = memory_process_stores().await;
    let registry: Arc<dyn ProcessRegistry> = stores.registry.clone();
    let continuations = Arc::clone(&stores.continuations);
    let registration = rerunnable_registration(process_id.as_str());
    registry
        .register_process(registration.clone())
        .await
        .expect("register the handing-over row");
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(BoundaryRunner),
                Arc::clone(&registry),
                Arc::clone(&continuations),
            )
            .serve(),
        )
        .build();
    stores
        .registry
        .fail_next_external_ref_write(PluginError::Session(
            "injected non-retryable reference write failure".to_string(),
        ));
    let output = invoke_process_workflow_endpoint(
        &endpoint,
        "run",
        process_id.as_str(),
        &segment_input(&registration, 0),
        true,
    )
    .await
    .expect("the segment ends inside its invocation");
    assert_eq!(
        restate_output_failure_message(&output),
        None,
        "the invocation publishes a terminal, it does not fail"
    );
    assert!(
        matches!(
            restate_output_json::<RestateProcessWorkflowOutput>(&output),
            Some(RestateProcessWorkflowOutput::Terminal { .. })
        ),
        "the segment delivers the process terminal"
    );
    assert!(
        restate_message_types(&output)
            .expect("decode the segment")
            .contains(&RESTATE_COMPLETE_PROMISE_COMMAND_MESSAGE_TYPE),
        "the root segment resolves the terminal promise awaiters wait on"
    );
    let record = registry
        .get_process(&process_id)
        .await
        .expect("read the failed row")
        .expect("the row stays registered");
    assert!(record.is_terminal(), "the process ended: {record:?}");
    assert_eq!(
        terminal_failure_code(&record).as_deref(),
        Some("process_segment_handover_write")
    );
    assert!(
        continuations
            .get_segment_handover(&process_id, 1)
            .await
            .expect("read the successor handover")
            .is_none(),
        "nothing was handed over"
    );
}

/// FIG-3809/FIG-3789: a later segment admitted with no handover to resume
/// from ends its process Failed, typed `process_segment_handover_missing`,
/// and delivers the terminal to the root workflow awaiters wait on.
#[tokio::test]
pub(super) async fn a_segment_with_no_handover_ends_the_process_failed_typed() {
    let process_id = ProcessId::from("segment-failure-handover-missing");
    let stores = memory_process_stores().await;
    let registry: Arc<dyn ProcessRegistry> = stores.registry.clone();
    let continuations = Arc::clone(&stores.continuations);
    let registration = rerunnable_registration(process_id.as_str());
    registry
        .register_process(registration.clone())
        .await
        .expect("register the row");
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(BoundaryRunner),
                Arc::clone(&registry),
                Arc::clone(&continuations),
            )
            .serve(),
        )
        .build();
    let suspended = invoke_endpoint_body_with_json_call_responses_then_suspend(
        &endpoint,
        "LashProcessWorkflow",
        "run",
        endpoint_protocol::encode_invocation_body(
            &format!("{}#1", process_id.as_str()),
            &segment_input(&registration, 1),
        )
        .expect("encode segment 1"),
        Vec::new(),
    )
    .await
    .expect("the segment suspends on its root delivery");
    assert_eq!(restate_output_failure_message(&suspended), None);
    assert!(
        restate_recorded_commands(&suspended)
            .expect("decode the segment")
            .iter()
            .any(
                |command| command.call.as_ref().is_some_and(|(service, handler)| {
                    service == "LashProcessWorkflow" && handler == "complete_terminal"
                })
            ),
        "the segment delivers the terminal to the root workflow"
    );
    let record = registry
        .get_process(&process_id)
        .await
        .expect("read the failed row")
        .expect("the row stays registered");
    assert!(record.is_terminal(), "the process ended: {record:?}");
    assert_eq!(
        terminal_failure_code(&record).as_deref(),
        Some("process_segment_handover_missing")
    );
}

/// The sweep submits every live row under its latest handover's key, whatever
/// the external reference says: a reference that already names the segment
/// no longer hides a segment whose workflow Restate lost.
#[tokio::test]
pub(super) async fn sweep_submits_the_latest_segment_even_when_its_reference_is_current() {
    let segment = HandedOverSegment::new("admission-sweep-current-ref").await;
    segment
        .registry
        .set_external_ref(
            &segment.registration.id,
            lash_core::ProcessExternalRef {
                backend: "restate".to_string(),
                id: format!("LashProcessWorkflow/{}", segment.key()),
                metadata: None,
                segment_ordinal: Some(1),
            },
        )
        .await
        .expect("the reference names segment 1");
    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "202 Accepted",
        body: r#"{"invocationId":"inv_current_ref","status":"PreviouslyAccepted"}"#,
    }])
    .await;
    let report = RestateProcessIngressRunner::new(
        base_url,
        Arc::clone(&segment.registry),
        Arc::clone(&segment.continuations),
    )
    .admit_pending_processes("admission-sweep")
    .await
    .expect("sweep the pending rows");
    server.await.expect("capture server");
    assert_eq!(report.admitted, vec![segment.registration.id.to_string()]);
    let requests = captured.lock_recover().clone();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(
        requests[0]
            .starts_with("POST /LashProcessWorkflow/admission-sweep-current-ref%231/run/send "),
        "the sweep addresses the latest segment's key: {}",
        requests[0]
    );
    assert!(
        requests[0].contains(&format!(
            "\"journal_version\":{RESTATE_PROCESS_JOURNAL_VERSION}"
        )),
        "the sweep stamps the journal generation: {}",
        requests[0]
    );
}

/// An input built for another generation of the handler's command prefix —
/// stamped with a retired version, or unstamped — is refused before the
/// handler journals anything: the process ends Abandoned with
/// `ResumeRefused { RetiredGeneration }` naming the generation, and the
/// runner is never asked for anything.
#[tokio::test]
pub(super) async fn a_retired_journal_generation_is_refused_before_any_command() {
    for (process_id, stamped) in [
        ("admission-retired-journal-v1", Some(1_u32)),
        ("admission-retired-journal-v2", Some(2_u32)),
        ("admission-retired-journal-unstamped", None),
    ] {
        let registry = process_registry();
        let registration = rerunnable_registration(process_id);
        registry
            .register_process(registration.clone())
            .await
            .expect("register the row");
        let runner = Arc::new(EffectRunner::default());
        let endpoint = Endpoint::builder()
            .bind(
                LashProcessWorkflowImpl::new_for_test(
                    Arc::clone(&runner),
                    Arc::clone(&registry),
                    continuation_store(),
                )
                .serve(),
            )
            .build();
        let mut input =
            serde_json::to_value(segment_input(&registration, 0)).expect("encode the input");
        match stamped {
            Some(version) => input["journal_version"] = serde_json::json!(version),
            None => {
                input
                    .as_object_mut()
                    .expect("the input is an object")
                    .remove("journal_version");
            }
        }
        let output = invoke_process_workflow_endpoint(&endpoint, "run", process_id, &input, true)
            .await
            .unwrap_or_default();
        assert_eq!(
            restate_recorded_commands(&output).map(|commands| {
                commands
                    .iter()
                    .filter(|command| command.message_type != 0x0401)
                    .count()
            }),
            Some(0),
            "{process_id}: nothing but the terminal output is journaled: {output:?}"
        );
        assert!(
            restate_output_failure_message(&output).is_some(),
            "{process_id}: the refusal is a terminal failure: {output:?}"
        );
        assert_eq!(runner.runs.load(Ordering::SeqCst), 0);
        let outcome = registry
            .get_process(&ProcessId::from(process_id))
            .await
            .expect("read the refused row")
            .and_then(|record| record.outcome)
            .expect("the refusal is stored");
        assert!(
            matches!(
                &outcome,
                ProcessAwaitOutput::Abandoned { evidence, .. }
                    if evidence.writer == lash_core::AbandonWriter::ResumeRefused {
                        reason: lash_core::ProcessResumeRefusal::RetiredGeneration {
                            found: format!(
                                "restate-process-journal-v{}",
                                stamped.unwrap_or(1)
                            ),
                        },
                    }
            ),
            "{process_id}: got {outcome:?}"
        );
    }
}

/// FIG-3818 → FIG-3820: after the SubstrateLost recovery stored the
/// process's `Abandoned` terminal, a zombie root execution (killed, purged,
/// still running on its deployment) can still park its handover and send
/// its successor. Segment 1's admission finds the handover and the root's
/// start, admits it, and drives the runner under a process that already
/// ended. The stored terminal is the revocation: segment 1's admission finds
/// it, runs nothing and delivers it to the root workflow (FIG-3820).
#[tokio::test]
pub(super) async fn a_zombie_successor_after_substrate_lost_recovery_runs_no_body() {
    let segment = HandedOverSegment::new("fig3818-zombie-successor").await;
    let abandoned = ProcessAwaitOutput::Abandoned {
        evidence: Box::new(lash_core::AbandonEvidence {
            writer: lash_core::AbandonWriter::ResumeRefused {
                reason: lash_core::ProcessResumeRefusal::SubstrateLost,
            },
            owner: Some(segment.root_start.owner.clone()),
            epoch_ms: 1,
        }),
        control: None,
    };
    segment
        .registry
        .complete_process(
            &segment.registration.id,
            abandoned.clone(),
            crate::process::workflow_key_authority(&segment.registration.id),
        )
        .await
        .expect("the recovery stores Abandoned");
    // The zombie's handover is already parked (the fixture); its successor
    // send lands as a fresh invocation of segment 1.
    let _ = segment.invoke_fresh(true).await;
    assert_eq!(
        segment.runs(),
        0,
        "a successor of a revoked execution must not run its body"
    );
    assert_eq!(segment.outcome().await, Some(abandoned));
}

/// A process whose root execution started under [`ROOT_EXECUTION`] and whose
/// root invocation's journal is gone: a fresh root invocation is its
/// SubstrateLost recovery, and the zombie root execution may still hand
/// segment 1 over. Both segments run on one endpoint over one store.
struct ZombieRoot {
    registration: ProcessRegistration,
    registry: Arc<dyn ProcessRegistry>,
    continuations: Arc<dyn lash_core::ProcessContinuationStore>,
    runner: Arc<EffectRunner>,
    endpoint: Endpoint,
}

impl ZombieRoot {
    async fn new(process_id: &str) -> Self {
        let (registry, continuations) = process_stores();
        let registration = rerunnable_registration(process_id);
        registry
            .register_process(registration.clone())
            .await
            .expect("register the process");
        let (authority, root_start) =
            invocation_started(&ProcessId::from(process_id), ROOT_EXECUTION, 1);
        registry
            .record_first_started_with_authority(
                &ProcessId::from(process_id),
                root_start,
                &authority,
            )
            .await
            .expect("record the zombie root execution's start");
        let runner = Arc::new(EffectRunner::default());
        let endpoint = Endpoint::builder()
            .bind(
                LashProcessWorkflowImpl::new_for_test(
                    Arc::clone(&runner),
                    Arc::clone(&registry),
                    Arc::clone(&continuations),
                )
                .serve(),
            )
            .build();
        Self {
            registration,
            registry,
            continuations,
            runner,
            endpoint,
        }
    }

    fn key(&self, segment_ordinal: u64) -> String {
        process_segment_workflow_key(&self.registration.id, segment_ordinal)
    }

    /// A fresh invocation of `segment_ordinal`, runs acknowledged or not.
    async fn invoke_fresh(&self, segment_ordinal: u64, complete_runs: bool) -> bytes::Bytes {
        invoke_process_workflow_endpoint(
            &self.endpoint,
            "run",
            &self.key(segment_ordinal),
            &segment_input(&self.registration, segment_ordinal),
            complete_runs,
        )
        .await
        .unwrap_or_default()
    }

    /// Restate's retry of the root invocation, with its first `journaled`
    /// commands acknowledged.
    async fn retry_root(&self, prior: &[u8], journaled: usize) -> bytes::Bytes {
        let body = encode_journal_retry(
            &self.key(0),
            &segment_input(&self.registration, 0),
            prior,
            journaled,
        )
        .expect("encode the acknowledged journal");
        invoke_process_workflow_body(&self.endpoint, "run", body, true)
            .await
            .unwrap_or_default()
    }

    /// The zombie root execution's handover step: the successor reference,
    /// then the handover, as `lash.segment.handover` writes them.
    async fn zombie_hands_over(&self) -> Result<(), PluginError> {
        self.registry
            .set_external_ref(
                &self.registration.id,
                lash_core::ProcessExternalRef {
                    backend: "restate".to_string(),
                    id: format!("LashProcessWorkflow/{}", self.key(1)),
                    metadata: None,
                    segment_ordinal: Some(1),
                },
            )
            .await?;
        self.continuations
            .put_segment_handover(
                &self.registration.id,
                lash_core::PersistedSegmentHandover {
                    segment_ordinal: 1,
                    writer: String::new(),
                    handover: lash_core::SegmentHandover {
                        reason: lash_core::BoundaryReason::JournalBudget,
                        program_hash: "program-v1".to_string(),
                        engine_state: vec![1],
                    },
                },
            )
            .await
    }

    async fn outcome(&self) -> Option<ProcessAwaitOutput> {
        self.registry
            .get_process(&self.registration.id)
            .await
            .expect("read the process")
            .expect("the process exists")
            .outcome
    }

    fn runs(&self) -> usize {
        self.runner.runs.load(Ordering::SeqCst)
    }

    /// Exactly one of "segment 1 ran" and "the recovery stored Abandoned".
    async fn assert_exactly_one_carrier(&self) {
        let abandoned = self
            .outcome()
            .await
            .is_some_and(|outcome| matches!(outcome, ProcessAwaitOutput::Abandoned { .. }));
        let segment_one_ran = self.runs() > 0;
        assert!(
            abandoned != segment_one_ran,
            "exactly one of segment 1 running ({segment_one_ran}) and the recovery's \
             Abandoned ({abandoned}): outcome {:?}",
            self.outcome().await
        );
    }
}

/// FIG-3820, the in-between order: the recovery's verdict is journaled
/// (SubstrateLost), then the zombie root hands over, then the recovery's
/// completion step runs. The step finds segment 1 named as the carrier in the
/// same transaction that would store Abandoned, records `HandedOver` and
/// stores nothing; segment 1 carries the process.
#[tokio::test]
pub(super) async fn a_zombie_handover_between_the_recovery_verdict_and_its_terminal_wins() {
    let root = ZombieRoot::new("fig3820-between").await;
    let verdict = root.invoke_fresh(0, false).await;
    assert_eq!(
        proposed_runs(&verdict),
        1,
        "the verdict proposed: {verdict:?}"
    );
    root.zombie_hands_over()
        .await
        .expect("the zombie hands over before any terminal");
    let recovered = root.retry_root(&verdict, 1).await;
    assert!(
        matches!(
            restate_output_json::<RestateProcessWorkflowOutput>(&recovered),
            Some(RestateProcessWorkflowOutput::SegmentChained {
                next_segment_ordinal: 1
            })
        ),
        "the recovery stands down for segment 1: {:?}",
        restate_error_message(&recovered)
    );
    assert_eq!(root.outcome().await, None, "the recovery stored nothing");
    root.invoke_fresh(1, true).await;
    assert_eq!(root.runs(), 1, "segment 1 carries the process");
    root.assert_exactly_one_carrier().await;
}

/// FIG-3820, recovery first: the recovery stores Abandoned, so the zombie's
/// handover step is refused typed at its successor reference, and a successor
/// send that lands anyway runs nothing.
#[tokio::test]
pub(super) async fn a_zombie_handover_after_the_recovery_terminal_is_refused_typed() {
    let root = ZombieRoot::new("fig3820-recovery-first").await;
    root.invoke_fresh(0, true).await;
    assert!(
        root.outcome()
            .await
            .is_some_and(|outcome| matches!(outcome, ProcessAwaitOutput::Abandoned { .. })),
        "the recovery stored Abandoned"
    );
    assert!(
        matches!(
            root.zombie_hands_over().await,
            Err(PluginError::ProcessAlreadyTerminal { .. })
        ),
        "the zombie's successor reference is refused typed"
    );
    root.invoke_fresh(1, true).await;
    assert_eq!(root.runs(), 0, "no successor body runs");
    root.assert_exactly_one_carrier().await;
}

/// FIG-3820, zombie first: the zombie hands over before the recovery's
/// verdict, so the verdict is Superseded; segment 1 carries the process.
#[tokio::test]
pub(super) async fn a_zombie_handover_before_the_recovery_verdict_supersedes_it() {
    let root = ZombieRoot::new("fig3820-zombie-first").await;
    root.zombie_hands_over()
        .await
        .expect("the zombie hands over before any terminal");
    root.invoke_fresh(0, true).await;
    assert_eq!(root.outcome().await, None, "the recovery stored nothing");
    root.invoke_fresh(1, true).await;
    assert_eq!(root.runs(), 1, "segment 1 carries the process");
    root.assert_exactly_one_carrier().await;
}

/// FIG-3820: a handover put on an ended process is refused typed, in the
/// transaction that would park it.
#[tokio::test]
pub(super) async fn a_handover_put_on_an_ended_process_is_refused_typed() {
    let root = ZombieRoot::new("fig3820-put-after-terminal").await;
    root.registry
        .complete_process(
            &root.registration.id,
            process_success(serde_json::json!("done")),
            crate::process::workflow_key_authority(&root.registration.id),
        )
        .await
        .expect("the process ends");
    let refused = root
        .continuations
        .put_segment_handover(
            &root.registration.id,
            lash_core::PersistedSegmentHandover {
                segment_ordinal: 1,
                writer: String::new(),
                handover: lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: "program-v1".to_string(),
                    engine_state: vec![1],
                },
            },
        )
        .await;
    assert!(
        matches!(refused, Err(PluginError::ProcessAlreadyTerminal { .. })),
        "{refused:?}"
    );
    assert_eq!(
        root.continuations
            .latest_segment_handover(&root.registration.id)
            .await
            .expect("read handovers"),
        None
    );
}
