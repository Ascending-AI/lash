//! FIG-3464: a Lashlang process's durable effect summary on the Restate
//! substrate. The effect is journaled in the invocation; the summary event is
//! a separate process-log write that a crash can lose. The replay
//! re-incorporates the journaled result and appends the summary exactly once,
//! and a divergent payload under the same effect key is refused without
//! reaching the program.

use super::*;
use lash_core::ProcessEventLogTestSupport as _;

use lashlang::testing::ast_builders as b;

pub(super) async fn counting_lashlang_registration(process_id: &ProcessId) -> ProcessRegistration {
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
    lashlang::LashlangArtifactStore::publish_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
        &lash_core::ArtifactOwner::host("restate-effect-summary"),
        &linked.artifact,
    )
    .await
    .expect("store effect-summary artifact");
    ProcessRegistration::new(
        process_id.clone(),
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked.artifact.module_ref.clone(),
            process_ref: linked
                .artifact
                .process_ref("worker")
                .expect("worker process ref")
                .clone(),
            host_requirements_ref: linked.artifact.host_requirements_ref.clone(),
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

fn invocation(process_id: &ProcessId) -> lash_core::ProcessExecutionWriteAuthority {
    lash_core::ProcessExecutionWriteAuthority::invocation(process_id, "effect-summary-invocation")
}

/// Runs the process's one Restate invocation against `context`'s journal.
pub(super) async fn run_invocation(
    registry: Arc<dyn ProcessRegistry>,
    executions: &Arc<AtomicUsize>,
    context: &Arc<ReplayableRecordingContext>,
    registration: &ProcessRegistration,
) -> Result<lash_core::ProcessRunOutcome, lash_core::PluginError> {
    let worker = recovery_worker_with_plugins(
        Arc::clone(&registry),
        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
        vec![counting_tool_plugin(Arc::clone(executions))],
    );
    let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(context));
    let scope = controller
        .scoped_effect_controller(
            recorded_process_admission(registry.as_ref(), &registration.id).await,
        )
        .expect("scope the process invocation");
    worker
        .run_process_segment_with_scoped_effect_controller(
            registration.clone(),
            ProcessExecutionContext::default(),
            invocation(&registration.id),
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

fn settled_success(outcome: &lash_core::ProcessRunOutcome) -> serde_json::Value {
    let lash_core::ProcessRunOutcome::Terminal { output } = outcome else {
        panic!("the replayed invocation terminates: {outcome:?}");
    };
    let ProcessAwaitOutput::Settled { output } = output.as_ref() else {
        panic!("the replayed invocation settles: {output:?}");
    };
    assert!(
        output.is_success(),
        "the program outcome is unchanged: {output:?}"
    );
    output.value_for_projection()
}

#[tokio::test]
async fn restate_crash_between_journaled_effect_and_summary_append_replays_once() {
    let process_id = ProcessId::from("restate-effect-summary-crash");
    let registry = process_registry();
    let registration = counting_lashlang_registration(&process_id).await;
    registry
        .register_process(registration.clone())
        .await
        .expect("register the effect-summary process");
    let executions = Arc::new(AtomicUsize::new(0));
    let context = Arc::new(ReplayableRecordingContext::default());

    // The tool attempt is journaled, then the summary append "crashes".
    let faults = Arc::new(lash_core::EffectSummaryAppendFaults::new(
        Arc::clone(&registry),
        lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
        usize::MAX,
    ));
    let crashed = run_invocation(
        Arc::clone(&faults) as Arc<dyn ProcessRegistry>,
        &executions,
        &context,
        &registration,
    )
    .await
    .expect_err("a failed summary append is an incorporation failure, not an outcome");
    assert!(
        crashed.to_string().contains("injected crash before the"),
        "{crashed}"
    );
    assert_eq!(faults.injected(), 1);
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert!(effect_outcomes(&registry, &process_id).await.is_empty());

    // Restate replays the invocation: the journaled attempt answers without
    // re-running the tool, and the summary is appended once.
    context.start_replay();
    let replayed = run_invocation(Arc::clone(&registry), &executions, &context, &registration)
        .await
        .expect("replay the invocation");
    assert_eq!(settled_success(&replayed), serde_json::json!(1));
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "the replay answered from the journal"
    );
    let outcomes = effect_outcomes(&registry, &process_id).await;
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        outcomes[0].payload,
        faults.refused()[0].payload,
        "the replay wrote exactly the record the crashed attempt meant to write"
    );

    // A second replay recovers the same event rather than appending again.
    let again = run_invocation(Arc::clone(&registry), &executions, &context, &registration)
        .await
        .expect("replay the invocation again");
    assert_eq!(settled_success(&again), serde_json::json!(1));
    let summarise = |events: &[lash_core::ProcessEvent]| {
        events
            .iter()
            .map(|event| (event.sequence, event.payload.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        summarise(&effect_outcomes(&registry, &process_id).await),
        summarise(&outcomes)
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn restate_replay_refuses_a_changed_summary_payload_without_reaching_the_program() {
    let process_id = ProcessId::from("restate-effect-summary-conflict");
    let registry = process_registry();
    let registration = counting_lashlang_registration(&process_id).await;
    registry
        .register_process(registration.clone())
        .await
        .expect("register the effect-summary process");
    let executions = Arc::new(AtomicUsize::new(0));
    let context = Arc::new(ReplayableRecordingContext::default());
    let faults = Arc::new(lash_core::EffectSummaryAppendFaults::new(
        Arc::clone(&registry),
        lash_core::PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
        usize::MAX,
    ));
    run_invocation(
        Arc::clone(&faults) as Arc<dyn ProcessRegistry>,
        &executions,
        &context,
        &registration,
    )
    .await
    .expect_err("the first summary append crashes");

    // A different record already holds the effect's key.
    let mut divergent = faults.refused()[0].clone();
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
    let refused = run_invocation(Arc::clone(&registry), &executions, &context, &registration)
        .await
        .expect_err("the replay refuses a changed payload under the same key");
    assert!(
        refused
            .to_string()
            .contains("conflicts with an existing event"),
        "the refusal is the replay-conflict rule, surfaced as an incorporation failure: {refused}"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let outcomes = effect_outcomes(&registry, &process_id).await;
    assert_eq!(outcomes.len(), 1, "nothing was appended over the conflict");
    assert_eq!(outcomes[0].payload["outcome_class"], "cancelled");
}
