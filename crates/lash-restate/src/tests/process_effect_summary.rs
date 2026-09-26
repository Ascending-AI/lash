//! A Lashlang process's durable effect summary on the Restate substrate
//! (FIG-3464), committed at run boundaries (FIG-3571, law L7).
//!
//! Each effect is journaled in its invocation; its summary occurrence stays
//! pending in the run until the run's next boundary write (a wait's enter or
//! clear, or the terminal completion) commits it in that write's own
//! transaction, and rides segment state across a segment boundary. A crash
//! before any boundary commits loses nothing the journal cannot rebuild: the
//! redrive replays its effects from the journal, re-derives the same pending
//! occurrences and commits them once. A divergent payload already under an
//! occurrence's key is refused without reaching the program.

use super::*;
use lash_core::ProcessEventLogTestSupport as _;

use lashlang::testing::ast_builders as b;

pub(super) async fn counting_lashlang_registration() -> ProcessRegistration {
    // process worker() {
    //   called = tools.recovery_count({ line: "summary" })
    //   finish called.executed
    // }
    let module = b::module(
        vec![b::process(
            "worker",
            Vec::new(),
            b::block(vec![
                b::assign(
                    "called",
                    b::module_call(
                        &["tools"],
                        "recovery_count",
                        vec![b::record(vec![("line", b::string("summary"))])],
                    ),
                ),
                b::finish(b::field(b::var("called"), "executed")),
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
        .expect("link counting tool operation");
    let linked = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(resources, lashlang::LashlangAbilities::default()),
    )
    .expect("link effect-summary process");
    lashlang::LashlangArtifacts::publish_module_artifact(
        &recovery_artifact_store(),
        &lash_core::ArtifactOwner::host("restate-effect-summary"),
        &linked.artifact,
    )
    .await
    .expect("store effect-summary artifact");
    ProcessRegistration::new(
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked.artifact.module_ref().clone(),
            process_ref: linked
                .artifact
                .process_ref("worker")
                .expect("worker process ref")
                .clone(),
            host_requirements_ref: linked.artifact.host_requirements_ref().clone(),
            process_name: "worker".to_string(),
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

pub(super) fn invocation(process_id: &ProcessId) -> lash_core::ProcessExecutionWriteAuthority {
    lash_core::ProcessExecutionWriteAuthority::invocation(
        process_id.clone(),
        "effect-summary-invocation",
    )
}

/// Runs the process's one Restate invocation against `context`'s journal.
pub(super) async fn run_invocation(
    registry: Arc<dyn ProcessRegistry>,
    executions: &Arc<AtomicUsize>,
    context: &Arc<ReplayableRecordingContext>,
    process_id: &ProcessId,
    registration: &ProcessRegistration,
) -> Result<lash_core::ProcessRunOutcome, lash_core::PluginError> {
    let worker = recovery_worker_with_plugins(
        Arc::clone(&registry),
        memory_session_store_factory().await,
        vec![counting_tool_plugin(Arc::clone(executions))],
    )
    .await;
    let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(context));
    let scope = controller
        .process_scope_for_test(recorded_process_admission(registry.as_ref(), process_id).await)
        .expect("scope the process invocation");
    worker
        .run_process_segment_with_scoped_effect_controller(
            process_id.clone(),
            registration.clone(),
            ProcessExecutionContext::default(),
            invocation(process_id),
            scope,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
}

pub(super) async fn effect_outcomes(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
) -> Vec<lash_core::ProcessEvent> {
    registry
        .full_event_window(process_id, 0)
        .await
        .expect("read the process log")
        .into_iter()
        .filter(|event| event.event_type == lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE)
        .collect()
}

/// The terminal the invocation proposed, and the summary it carried.
fn terminal(
    outcome: lash_core::ProcessRunOutcome,
) -> (
    ProcessAwaitOutput,
    Vec<lash_core::ProcessEventAppendRequest>,
) {
    let lash_core::ProcessRunOutcome::Terminal { output, prelude } = outcome else {
        panic!("the invocation terminates: {outcome:?}");
    };
    (*output, prelude)
}

/// process worker() {
///   for (const line of ten lines) await tools.recovery_count({ line });
///   await waitSignal("go");
///   return (await tools.recovery_count({ line: "after" })).executed;
/// }
///
/// Ten occurrences of the loop's effect node (eight recorded one by one, two
/// counted as omitted), a wait whose enter commits the eight, and a last
/// effect the terminal batch commits with the omission record.
async fn looping_waiting_registration() -> ProcessRegistration {
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
        .expect("link counting tool operation");
    let linked = lash_typescript::link(
        r#"
        const worker = async () => {
          for (const line of ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]) {
            await tools.recovery_count({ line: line });
          }
          await waitSignal("go");
          const last = await tools.recovery_count({ line: "after" });
          return last.executed;
        };
        finish(null);
        "#,
        &lashlang::LashlangHostEnvironment::new(resources, lashlang::LashlangAbilities::all()),
    )
    .expect("link the looping, waiting process");
    lashlang::LashlangArtifacts::publish_module_artifact(
        &recovery_artifact_store(),
        &lash_core::ArtifactOwner::host("restate-effect-summary"),
        &linked.artifact,
    )
    .await
    .expect("store the looping, waiting artifact");
    let worker = sole_lifted_process_name(&linked.artifact);
    ProcessRegistration::new(
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked.artifact.module_ref().clone(),
            process_ref: linked
                .artifact
                .process_ref(&worker)
                .expect("worker process ref")
                .clone(),
            host_requirements_ref: linked.artifact.host_requirements_ref().clone(),
            process_name: worker,
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

/// What a run left in the process log: its summary occurrences, logically
/// (node, occurrence, operation, class, and replay key with the process id
/// abstracted, in log order), and its omission records.
#[derive(Debug, PartialEq)]
struct SummaryLog {
    occurrences: Vec<lash_core::ProcessEffectSummaryOccurrence>,
    omissions: Vec<lash_core::ProcessEffectOmissions>,
}

async fn summary_log(registry: &Arc<dyn ProcessRegistry>, process_id: &ProcessId) -> SummaryLog {
    let events = registry
        .full_event_window(process_id, 0)
        .await
        .expect("read the process log");
    SummaryLog {
        occurrences: events
            .iter()
            .filter(|event| event.event_type == lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE)
            .map(|event| {
                let mut occurrence =
                    lash_core::ProcessEffectSummaryOccurrence::decode(event.payload.clone())
                        .expect("decode a summary occurrence");
                occurrence.replay_key = occurrence
                    .replay_key
                    .replace(process_id.as_str(), "<process>");
                occurrence
            })
            .collect(),
        omissions: events
            .iter()
            .filter(|event| event.event_type == lash_core::PROCESS_EFFECT_OMISSIONS_EVENT_TYPE)
            .map(|event| {
                lash_core::ProcessEffectOmissions::decode(event.payload.clone())
                    .expect("decode the omission record")
            })
            .collect(),
    }
}

/// One drive of the looping, waiting process to its stored terminal.
struct SummaryDrive {
    registry: Arc<dyn ProcessRegistry>,
    process_id: ProcessId,
    stored: ProcessAwaitOutput,
    executions: usize,
    boundaries: usize,
    /// Summary occurrences in the log when the first segment handed over.
    logged_at_first_boundary: Option<usize>,
    /// Invocations that failed before a boundary committed and were replayed.
    interrupted: usize,
    faults: Arc<lash_core::EffectSummaryAppendFaults>,
}

/// Drive the looping, waiting process segment by segment, each segment its
/// own invocation over its own journal, `budget` effects per segment, every
/// write through `faults` (built over the plain registry). An invocation that
/// fails is the crash it stands for: the journal keeps what it settled, loses
/// the unsettled step, and the retry replays it. The terminal is stored by the
/// completion step's body.
async fn drive_to_terminal(
    budget: u64,
    faults: impl FnOnce(Arc<dyn ProcessRegistry>) -> lash_core::EffectSummaryAppendFaults,
) -> SummaryDrive {
    let registry = process_registry();
    let registration = looping_waiting_registration().await;
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register the looping, waiting process")
        .id;
    let faults = Arc::new(faults(Arc::clone(&registry)));
    let writes: Arc<dyn ProcessRegistry> = Arc::clone(&faults) as Arc<dyn ProcessRegistry>;
    let executions = Arc::new(AtomicUsize::new(0));
    let worker = recovery_worker_with_plugins(
        Arc::clone(&writes),
        memory_session_store_factory().await,
        vec![counting_tool_plugin(Arc::clone(&executions))],
    )
    .await;
    let signal_key = restate_await_event_key_for_authority(
        &test_restate_authority_id(),
        &ExecutionScope::process(process_id.clone()),
        AwaitEventWaitIdentity::process_signal(&process_id, "go", 1),
    )
    .expect("the signal wait's key");
    let run = |context: Arc<ReplayableRecordingContext>,
               handover: Option<lash_core::SegmentHandover>| {
        let worker = worker.clone();
        let registry = Arc::clone(&registry);
        let registration = registration.clone();
        let process_id = process_id.clone();
        async move {
            let controller = RestateRuntimeEffectController::with_options_for_test(
                context,
                RestateEffectControllerOptions::default()
                    .process_segment_drive()
                    .segment_effect_budget(budget),
            );
            worker
                .run_process_segment_with_scoped_effect_controller(
                    process_id.clone(),
                    registration,
                    ProcessExecutionContext::default(),
                    invocation(&process_id),
                    controller
                        .process_scope_for_test(
                            recorded_process_admission(registry.as_ref(), &process_id).await,
                        )
                        .expect("scope the segment"),
                    tokio_util::sync::CancellationToken::new(),
                    handover,
                )
                .await
        }
    };
    // The retry of a crashed invocation: its settled steps replay, the step
    // it was inside never journaled, and it continues live from there.
    let retry = |context: &Arc<ReplayableRecordingContext>| {
        context.records.lock_recover().retain(|_, bytes| {
            !serde_json::from_slice::<serde_json::Value>(bytes)
                .is_ok_and(|value| value.get("Err").is_some())
        });
        context.start_replay_allowing_journal_extension();
    };
    let mut handover = None;
    let mut boundaries = 0;
    let mut logged_at_first_boundary = None;
    let mut interrupted = 0;
    let stored = loop {
        let context = Arc::new(ReplayableRecordingContext::default());
        context
            .events
            .resolve_durable_event(RestateDurableWaitResolveRequest {
                key: signal_key.clone(),
                resolution: Resolution::Ok(serde_json::json!({ "go": true })),
            });
        let outcome = match run(Arc::clone(&context), handover.clone()).await {
            Ok(outcome) => outcome,
            Err(crashed) => {
                assert!(
                    crashed.to_string().contains("injected crash before the"),
                    "only the injected crash interrupts the drive: {crashed}"
                );
                interrupted += 1;
                retry(&context);
                run(Arc::clone(&context), handover.clone())
                    .await
                    .expect("the retried invocation runs on")
            }
        };
        match outcome {
            lash_core::ProcessRunOutcome::SegmentBoundary(next) => {
                boundaries += 1;
                if logged_at_first_boundary.is_none() {
                    logged_at_first_boundary =
                        Some(summary_log(&registry, &process_id).await.occurrences.len());
                }
                handover = Some(next);
            }
            outcome => {
                let (output, prelude) = terminal(outcome);
                match crate::process::complete_process_outcome(
                    &writes,
                    &process_id,
                    output,
                    prelude,
                )
                .await
                {
                    Ok(stored) => break stored,
                    Err(crashed) => {
                        assert!(
                            crashed.to_string().contains("injected crash before the"),
                            "only the injected crash interrupts the completion: {crashed}"
                        );
                        // The completion step never journaled: the retry
                        // replays the runner and proposes the same terminal.
                        interrupted += 1;
                        retry(&context);
                        let (output, prelude) = terminal(
                            run(Arc::clone(&context), handover.clone())
                                .await
                                .expect("the retried invocation replays its runner"),
                        );
                        break crate::process::complete_process_outcome(
                            &writes,
                            &process_id,
                            output,
                            prelude,
                        )
                        .await
                        .expect("the retried completion stores the terminal");
                    }
                }
            }
        }
    };
    SummaryDrive {
        registry,
        process_id,
        stored,
        executions: executions.load(Ordering::SeqCst),
        boundaries,
        logged_at_first_boundary,
        interrupted,
        faults,
    }
}

fn plain(registry: Arc<dyn ProcessRegistry>) -> lash_core::EffectSummaryAppendFaults {
    lash_core::EffectSummaryAppendFaults::new(
        registry,
        lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
        0,
    )
}

/// L7: the summary commits once per run boundary, never once per
/// occurrence. The loop's eight recorded occurrences ride segment state
/// across the segments the budget cuts and commit with the wait's enter; the
/// last occurrence and the omission record commit with the terminal.
#[tokio::test]
async fn a_run_commits_its_summary_once_per_boundary() {
    let drive = Box::pin(drive_to_terminal(4, plain)).await;
    assert_eq!(drive.executions, 11, "every effect ran once");
    assert!(
        drive.boundaries >= 2,
        "the budget cut the run into segments"
    );
    assert_eq!(
        drive.logged_at_first_boundary,
        Some(0),
        "a segment boundary commits nothing: the pending occurrences ride its handover"
    );
    assert_eq!(
        drive.faults.summary_writes(),
        2,
        "one write per boundary that had a summary: the wait's enter and the terminal"
    );
    let log = summary_log(&drive.registry, &drive.process_id).await;
    assert_eq!(
        log.occurrences.len(),
        9,
        "eight loop occurrences and the last"
    );
    assert_eq!(
        log.omissions,
        vec![lash_core::ProcessEffectOmissions::new(
            [(
                log.occurrences[0].node_id.clone(),
                lash_core::ProcessEffectOmittedCounts {
                    success: 2,
                    failure: 0,
                    cancelled: 0,
                },
            )]
            .into()
        )],
        "the two loop occurrences past the cap are counted"
    );
    // The eight loop occurrences commit in the wait's batch, ahead of its
    // `process.waiting`; the terminal batch closes with the omission record
    // and the terminal.
    let events = drive
        .registry
        .full_event_window(&drive.process_id, 0)
        .await
        .expect("read the process log");
    let kinds = events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>();
    let waiting = kinds
        .iter()
        .position(|kind| *kind == "process.waiting")
        .expect("the process waited");
    assert!(
        kinds[waiting - 8..waiting]
            .iter()
            .all(|kind| *kind == lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE),
        "the wait's batch carries the loop's occurrences: {kinds:?}"
    );
    assert_eq!(
        kinds[kinds.len() - 3..],
        [
            lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
            lash_core::PROCESS_EFFECT_OMISSIONS_EVENT_TYPE,
            "process.completed",
        ],
        "the terminal batch: the last occurrence, the omission record, the terminal"
    );
}

/// L7: a crash before each boundary that carries a summary (the wait's
/// enter, reached from a successor segment carrying its predecessor's pending
/// occurrences, and the terminal completion) loses nothing: the retry replays
/// its effects from the journal, re-derives the same pending occurrences and
/// commits them once. The logical summary and the omission totals equal an
/// uninterrupted run's, segmented or not.
#[tokio::test]
async fn an_interrupted_boundary_commits_the_uninterrupted_summary() {
    let uninterrupted = Box::pin(drive_to_terminal(4, plain)).await;
    let expected = summary_log(&uninterrupted.registry, &uninterrupted.process_id).await;
    let unsegmented = Box::pin(drive_to_terminal(10_000, plain)).await;
    assert_eq!(unsegmented.boundaries, 0);
    assert_eq!(
        summary_log(&unsegmented.registry, &unsegmented.process_id).await,
        expected,
        "segment boundaries do not move the summary"
    );
    for flush_point in 0..uninterrupted.faults.summary_writes() {
        let drive = Box::pin(drive_to_terminal(4, |registry| {
            lash_core::EffectSummaryAppendFaults::new(
                registry,
                lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
                1,
            )
            .after(flush_point)
        }))
        .await;
        assert_eq!(
            drive.faults.injected(),
            1,
            "flush point {flush_point} was interrupted"
        );
        assert_eq!(drive.interrupted, 1);
        assert_eq!(
            drive.executions, 11,
            "flush point {flush_point}: the retry replayed its effects from the journal"
        );
        assert_eq!(drive.stored, uninterrupted.stored);
        assert_eq!(
            summary_log(&drive.registry, &drive.process_id).await,
            expected,
            "flush point {flush_point}: the summary equals the uninterrupted run's"
        );
    }
}

/// A different record already under an occurrence's key is refused at the
/// boundary that would commit it: the write commits nothing, the program never
/// observes the refusal, and the run reports it as an incorporation failure.
#[tokio::test]
async fn a_boundary_refuses_a_changed_summary_payload_without_reaching_the_program() {
    let registry = process_registry();
    let registration = counting_lashlang_registration().await;
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register the effect-summary process")
        .id;
    let executions = Arc::new(AtomicUsize::new(0));
    let context = Arc::new(ReplayableRecordingContext::default());
    let (_, prelude) = terminal(
        run_invocation(
            Arc::clone(&registry),
            &executions,
            &context,
            &process_id,
            &registration,
        )
        .await
        .expect("run the invocation"),
    );
    let occurrence = prelude
        .into_iter()
        .find(|request| request.event_type == lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE)
        .expect("the terminal batch carries the occurrence");

    // A different record already holds the effect's key.
    let mut divergent = occurrence.clone();
    divergent.payload["outcome_class"] = serde_json::json!("cancelled");
    registry
        .append_event_with_authority(
            &process_id,
            divergent,
            &invocation(&process_id).bind_attempt(1),
        )
        .await
        .expect("seed a divergent record under the effect key");

    context.start_replay();
    let (output, prelude) = terminal(
        run_invocation(
            Arc::clone(&registry),
            &executions,
            &context,
            &process_id,
            &registration,
        )
        .await
        .expect("the replay proposes the recorded terminal"),
    );
    let refused = crate::process::complete_process_outcome(&registry, &process_id, output, prelude)
        .await
        .expect_err("the terminal batch refuses a changed payload under the same key");
    assert!(
        refused
            .to_string()
            .contains("conflicts with an existing event"),
        "the refusal is the replay-conflict rule: {refused}"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let outcomes = effect_outcomes(&registry, &process_id).await;
    assert_eq!(outcomes.len(), 1, "nothing was appended over the conflict");
    assert_eq!(outcomes[0].payload["outcome_class"], "cancelled");
    let record = registry
        .get_process(&process_id)
        .await
        .expect("read the process")
        .expect("the process is retained");
    assert!(
        !record.is_terminal(),
        "the refused batch committed no terminal either"
    );
}
