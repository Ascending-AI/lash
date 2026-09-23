use super::*;
use lash_core::ProcessEventLogTestSupport as _;

use lashlang::testing::ast_builders as b;

#[tokio::test]
pub(super) async fn sqlite_process_recovery_rebuilds_snapshot_plugin_options_after_worker_reopen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let process_db = temp.path().join("processes.db");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        temp.path().join("sessions"),
    )) as Arc<dyn lash_core::SessionStoreFactory>;
    let registry_a = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("open registry"),
    ) as Arc<dyn ProcessRegistry>;
    let env_ref = persist_snapshot_recovery_env_ref("tool-authority:sha256:ok").await;
    registry_a
        .register_process(
            snapshot_lashlang_registration(&ProcessId::from("snapshot-ok"), env_ref).await,
        )
        .await
        .expect("register snapshot-backed process");
    drop(registry_a);

    let registry_b = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("reopen registry"),
    ) as Arc<dyn ProcessRegistry>;
    let worker_b = recovery_worker_with_plugins(
        Arc::clone(&registry_b),
        store_factory,
        vec![snapshot_recovery_tool_factory()],
    );
    let _ = worker_b
        .drive_pending_processes()
        .await
        .expect("recover snapshot-backed process");

    assert_eq!(
        lash_core::NativeProcessWork::for_registry(Arc::clone(&registry_b))
            .await_terminal(&ProcessId::from("snapshot-ok"))
            .await
            .expect("await recovered snapshot-backed process"),
        process_success(serde_json::json!("snapshot:restored"))
    );
}

struct InvalidLashlangBindingTool;

impl InvalidLashlangBindingTool {
    fn definition() -> lash_core::ToolDefinition {
        let mut definition = lash_core::ToolDefinition::raw(
            "tool:invalid_lashlang_binding",
            "invalid_lashlang_binding",
            "Malformed Lashlang binding fixture.",
            serde_json::json!({ "type": "object" }),
            serde_json::Value::Null,
        );
        definition.manifest.bindings.insert(
            lash_lashlang_runtime::TYPESCRIPT_TOOL_BINDING_KEY.to_string(),
            serde_json::json!({ "not": "a tool binding" }),
        );
        definition
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for InvalidLashlangBindingTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "invalid_lashlang_binding").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        unreachable!("the malformed binding must fail before tool execution")
    }
}

fn invalid_lashlang_binding_factory() -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(lash_core::plugin::StaticPluginFactory::new(
        "invalid-lashlang-binding",
        lash_core::facade_support::PluginSpec::new()
            .with_tool_provider(Arc::new(InvalidLashlangBindingTool)),
    ))
}

fn mutate_snapshot_lashlang_input(
    registration: &mut ProcessRegistration,
    mutate: impl FnOnce(&mut lash_lashlang_runtime::LashlangProcessInput),
) {
    let ProcessInput::Engine { payload, .. } = Arc::make_mut(&mut registration.input) else {
        panic!("snapshot registration must carry a Lashlang engine input")
    };
    let mut input = lash_lashlang_runtime::LashlangProcessInput::from_payload(payload.clone())
        .expect("decode snapshot Lashlang input");
    mutate(&mut input);
    registration.identity = input.process_identity();
    *payload = serde_json::to_value(input).expect("encode mutated snapshot Lashlang input");
}

#[tokio::test]
pub(super) async fn sqlite_process_recovery_preserves_lashlang_admission_failure_codes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let process_db = temp.path().join("processes.db");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        temp.path().join("sessions"),
    )) as Arc<dyn lash_core::SessionStoreFactory>;
    let registry_a = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("open registry"),
    ) as Arc<dyn ProcessRegistry>;
    let env_ref = persist_snapshot_recovery_env_ref("tool-authority:sha256:revoked").await;
    let mut requirements_mismatch = snapshot_lashlang_registration(
        &ProcessId::from("snapshot-requirements-mismatch"),
        env_ref.clone(),
    )
    .await;
    mutate_snapshot_lashlang_input(&mut requirements_mismatch, |input| {
        input.host_requirements_ref =
            lashlang::HostRequirementsRef::new(&lashlang::ContentHash::new("mismatch"));
    });
    registry_a
        .register_process(requirements_mismatch)
        .await
        .expect("register host-requirements mismatch");

    let mut process_ref_mismatch = snapshot_lashlang_registration(
        &ProcessId::from("snapshot-process-ref-mismatch"),
        env_ref.clone(),
    )
    .await;
    mutate_snapshot_lashlang_input(&mut process_ref_mismatch, |input| {
        input.process_ref = lashlang::ProcessRef::new(lashlang::ContentHash::new("mismatch"), 0);
    });
    registry_a
        .register_process(process_ref_mismatch)
        .await
        .expect("register process-ref mismatch");

    registry_a
        .register_process(
            snapshot_lashlang_registration(
                &ProcessId::from("snapshot-invalid-host-environment"),
                env_ref.clone(),
            )
            .await,
        )
        .await
        .expect("register invalid host environment");
    drop(registry_a);

    let registry_b = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("reopen registry"),
    ) as Arc<dyn ProcessRegistry>;
    let worker_b = recovery_worker_with_plugins(
        Arc::clone(&registry_b),
        Arc::clone(&store_factory),
        vec![
            snapshot_recovery_tool_factory(),
            invalid_lashlang_binding_factory(),
        ],
    );
    let _ = worker_b
        .drive_pending_processes()
        .await
        .expect("drive immutable and invalid-host admission failures");

    let incompatible_id = ProcessId::from("snapshot-incompatible-host-environment");
    registry_b
        .register_process(snapshot_lashlang_registration(&incompatible_id, env_ref).await)
        .await
        .expect("register incompatible host environment");
    let worker_c = recovery_worker_with_plugins(
        Arc::clone(&registry_b),
        store_factory,
        vec![snapshot_recovery_tool_factory()],
    );
    let _ = worker_c
        .drive_pending_processes()
        .await
        .expect("drive incompatible-host admission failure");

    for (process_id, expected_code) in [
        (
            "snapshot-requirements-mismatch",
            "process_host_requirements_mismatch",
        ),
        ("snapshot-process-ref-mismatch", "process_ref_mismatch"),
        (
            "snapshot-invalid-host-environment",
            "process_host_environment_invalid",
        ),
        (
            "snapshot-incompatible-host-environment",
            "process_host_environment_incompatible",
        ),
    ] {
        let await_output = lash_core::NativeProcessWork::for_registry(Arc::clone(&registry_b))
            .await_terminal(&ProcessId::from(process_id))
            .await
            .unwrap_or_else(|error| panic!("await terminal {process_id}: {error}"));
        let ProcessAwaitOutput::Settled { output } = await_output else {
            panic!("expected {process_id} failure, got {await_output:#?}");
        };
        let lash_core::ToolCallOutcome::Failure(failure) = output.outcome else {
            panic!("expected {process_id} failure, got {output:#?}");
        };
        assert_eq!(failure.code, expected_code, "{process_id}");
        if process_id == "snapshot-incompatible-host-environment" {
            assert!(
                failure
                    .message
                    .contains("module `tools` does not expose operation `snapshot_echo`"),
                "{}",
                failure.message
            );
        }
    }
}

/// Build a durable registration for a trigger-started Lashlang engine process.
///
/// A trigger-started process carries the trigger route's engine payload and
/// provenance whose `caused_by` is the
/// trigger occurrence that fired it — distinct from a turn-started process, whose
/// provenance traces to a live turn/tool call. The module artifact is stored
/// in the process-global in-memory artifact store, mirroring how a trigger
/// route's linked module is published before the process runs; that store
/// survives the registry/worker reopen within a single test process.
pub(super) async fn trigger_lashlang_registration(
    process_id: &ProcessId,
    resource: &str,
) -> ProcessRegistration {
    // process notify(resource: str) { finish { triggered: resource } }
    let module = b::module(
        vec![b::process(
            "notify",
            vec![b::param("resource", lashlang::TypeExpr::Str)],
            b::finish(b::record(vec![("triggered", b::var("resource"))])),
        )],
        Vec::new(),
    );
    let linked_module = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::new(),
            lashlang::LashlangAbilities::all(),
        ),
    )
    .expect("link lashlang trigger module");
    lashlang::LashlangArtifactStore::publish_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
        &lash_core::ArtifactOwner::host("restate-recovery-test"),
        &linked_module.artifact,
    )
    .await
    .expect("store lashlang trigger module artifact");
    let process_ref = linked_module
        .artifact
        .process_ref("notify")
        .expect("notify process ref")
        .clone();
    let mut args = serde_json::Map::new();
    args.insert("resource".to_string(), serde_json::json!(resource));
    let env_ref = persist_recovery_env_ref().await;
    ProcessRegistration::new(
        process_id,
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked_module.artifact.module_ref().clone(),
            process_ref,
            host_requirements_ref: linked_module.artifact.host_requirements_ref().clone(),
            process_name: "notify".to_string(),
            args,
        }),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::session(lash_core::SessionScope::new("root")).with_caused_by(
            Some(lash_core::CausalRef::SessionNode {
                session_id: SessionId::from("root"),
                node_id: "trigger:resource.updated".to_string(),
            }),
        ),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(env_ref))
}

/// The name the linker lifted a module's sole process arrow to.
///
/// FIG-2999 made a top-level `const` arrow a process *literal*, so the linked
/// artifact names it by its derived lift identity, not by the binding the
/// source spelled. A fixture that wants "the process this module declares"
/// asks the artifact rather than repeating a name the source no longer owns.
fn sole_lifted_process_name(artifact: &lashlang::ModuleArtifact) -> String {
    let mut processes =
        artifact
            .ir()
            .declarations
            .iter()
            .filter_map(|declaration| match declaration {
                lashlang::Declaration::Process(process) => Some(process.name.to_string()),
                _ => None,
            });
    let name = processes
        .next()
        .expect("the linked module declares one process");
    assert!(
        processes.next().is_none(),
        "the fixture module declares exactly one process"
    );
    name
}

pub(super) async fn typescript_process_registration(process_id: &ProcessId) -> ProcessRegistration {
    let linked = lash_typescript::link(
        r#"
        const worker = async () => { return { ok: true }; };
        finish(null);
        "#,
        &lashlang::LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::new(),
            lashlang::LashlangAbilities::all(),
        ),
    )
    .expect("link TypeScript process");
    lashlang::LashlangArtifactStore::publish_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
        &lash_core::ArtifactOwner::host("restate-recovery-test"),
        &linked.artifact,
    )
    .await
    .expect("store TypeScript artifact");
    let worker = sole_lifted_process_name(&linked.artifact);
    let process = linked
        .artifact
        .ir()
        .process(&worker)
        .expect("worker process declaration");
    let env_ref = persist_recovery_env_ref().await;
    ProcessRegistration::new(
        process_id,
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked.artifact.module_ref().clone(),
            process_ref: linked
                .artifact
                .process_ref(&worker)
                .expect("worker process ref")
                .clone(),
            host_requirements_ref: linked.artifact.host_requirements_ref().clone(),
            process_name: worker.clone(),
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
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_signal_event_types(
        process,
    ))
    .with_execution_env_ref(Some(env_ref))
}

pub(super) async fn sleeping_process_registration(process_id: &ProcessId) -> ProcessRegistration {
    let environment = lashlang::LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        lashlang::LashlangAbilities::all(),
    );
    let linked = lash_typescript::link(
        r#"
        const worker = async () => {
          await sleep(300000);
          return "completed after wake";
        };
        finish(null);
        "#,
        &environment,
    )
    .expect("link sleeping TypeScript process");
    lashlang::LashlangArtifactStore::publish_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
        &lash_core::ArtifactOwner::host("restate-recovery-test"),
        &linked.artifact,
    )
    .await
    .expect("store sleeping process artifact");
    let worker = sole_lifted_process_name(&linked.artifact);
    let env_ref = persist_recovery_env_ref().await;
    ProcessRegistration::new(
        process_id,
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
    .with_execution_env_ref(Some(env_ref))
}

pub(super) async fn sleeping_then_tool_process_registration(
    process_id: &ProcessId,
) -> ProcessRegistration {
    // process worker() {
    //   sleep for "5m"
    //   called = await tools.snapshot_echo({ line: "after wake" })?
    //   finish called.echo
    // }
    let module = b::module(
        vec![b::process(
            "worker",
            Vec::new(),
            b::block(vec![
                b::sleep_for(b::string("5m")),
                b::assign(
                    "called",
                    b::module_call(
                        &["tools"],
                        "snapshot_echo",
                        vec![b::record(vec![("line", b::string("after wake"))])],
                    ),
                ),
                b::finish(b::field(b::var("called"), "echo")),
            ]),
        )],
        Vec::new(),
    );
    let contract = SnapshotRecoveryTool::definition().contract();
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_module_operation_contract(
            ["tools"],
            "Tools",
            "snapshot_echo",
            "tool:snapshot_echo",
            &lashlang::OperationContract::new(
                contract.input_schema.canonical().clone(),
                contract.output_schema.canonical().clone(),
            ),
        )
        .expect("link post-wake tool operation");
    let linked = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(
            resources,
            lashlang::LashlangAbilities::default().with_sleep(),
        ),
    )
    .expect("link sleeping post-wake-effect process");
    lashlang::LashlangArtifactStore::publish_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
        &lash_core::ArtifactOwner::host("restate-recovery-test"),
        &linked.artifact,
    )
    .await
    .expect("store sleeping post-wake-effect artifact");
    let env_ref = persist_snapshot_recovery_env_ref("tool-authority:sha256:ok").await;
    ProcessRegistration::new(
        process_id,
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
    .with_execution_env_ref(Some(env_ref))
}

#[tokio::test]
pub(super) async fn process_sleep_wake_settles_recorded_cancel_before_resuming() {
    let process_id = ProcessId::from("sleep-cancel-typescript");
    let (registry, continuations) = process_stores();
    let registration = sleeping_process_registration(&process_id).await;
    registry
        .register_process(registration.clone())
        .await
        .expect("register sleeping process");
    let worker = recovery_worker(
        Arc::clone(&registry),
        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
    );
    let workflow = Arc::new(LashProcessWorkflowImpl::new_for_test(
        Arc::new(RestateCoreProcessRunner::new(worker)),
        Arc::clone(&registry),
        continuations,
    ));
    let context = Arc::new(ReplayableRecordingContext::default());
    context.park_sleeps();
    let execution_id = "sleep-cancel-invocation-typescript".to_string();
    let execution_write_authority =
        lash_core::ProcessExecutionWriteAuthority::invocation(&process_id, &execution_id);
    let run = {
        let workflow = Arc::clone(&workflow);
        let context = Arc::clone(&context);
        let process_id = process_id.clone();
        tokio::spawn(async move {
            let controller = RestateRuntimeEffectController::new_for_test(context);
            workflow
                .run_registration(
                    registration,
                    ProcessExecutionContext::default()
                        .with_execution_write_authority(execution_write_authority),
                    controller
                        .scoped_effect_controller(durable_admission(&ExecutionScope::process(
                            &process_id,
                        )))
                        .expect("sleeping process scope"),
                    0,
                    None,
                    pending_process_cancel_signal(),
                )
                .await
        })
    };

    context.await_sleep_started().await;
    registry
        .append_event(
            &process_id,
            lash_core::ProcessEventAppendRequest::cancel_requested(
                &registry
                    .resolve_process_ref(&process_id)
                    .await
                    .expect("retained cancellation target"),
                &lash_core::CancelRequest::new(
                    lash_core::CancelOrigin::OperatorRequested,
                    "actor:fixture:process_sleep_wake_settles_recorded_cancel_before_resuming",
                    11,
                ),
            ),
        )
        .await
        .expect("commit cancel before wake");
    context.commit_process_cancel();
    context.release_sleep();

    let outcome = run
        .await
        .expect("join sleeping process")
        .expect("run sleeping process");
    assert!(
        matches!(
            outcome,
            lash_core::ProcessRunOutcome::Terminal { ref output, .. }
                if output.terminal_status() == Some(lash_core::ProcessStatus::Cancelled)
        ),
        "the process resumed past its wake and produced {outcome:#?}"
    );
    assert_eq!(
        registry
            .get_process(&process_id)
            .await
            .expect("read sleeping process")
            .expect("sleeping process remains registered")
            .status,
        lash_core::ProcessStatus::Cancelled,
        "process terminal status"
    );
}

#[tokio::test]
pub(super) async fn process_sleep_wake_verdict_failure_retries_before_settling_recorded_cancel() {
    let process_id = "sleep-cancel-read-retry";
    let storage = Arc::new(lash_core::TestLocalProcessRegistry::default());
    let registry = Arc::clone(&storage) as Arc<dyn ProcessRegistry>;
    let continuations = Arc::clone(&storage) as Arc<dyn lash_core::ProcessContinuationStore>;
    let registration = sleeping_process_registration(&ProcessId::from(process_id)).await;
    registry
        .register_process(registration.clone())
        .await
        .expect("register sleeping process");
    let worker = recovery_worker(
        Arc::clone(&registry),
        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
    );
    let workflow = Arc::new(LashProcessWorkflowImpl::new_for_test(
        Arc::new(RestateCoreProcessRunner::new(worker)),
        Arc::clone(&registry),
        continuations,
    ));
    let context = Arc::new(ReplayableRecordingContext::default());
    context.park_sleeps();
    let execution_write_authority = lash_core::ProcessExecutionWriteAuthority::invocation(
        process_id,
        "sleep-cancel-read-retry-invocation",
    );
    let first_run = {
        let workflow = Arc::clone(&workflow);
        let context = Arc::clone(&context);
        let registration = registration.clone();
        let execution_write_authority = execution_write_authority.clone();
        tokio::spawn(async move {
            let controller = RestateRuntimeEffectController::new_for_test(context);
            workflow
                .run_registration(
                    registration,
                    ProcessExecutionContext::default()
                        .with_execution_write_authority(execution_write_authority),
                    controller
                        .scoped_effect_controller(durable_admission(&ExecutionScope::process(
                            process_id,
                        )))
                        .expect("sleeping process scope"),
                    0,
                    None,
                    pending_process_cancel_signal(),
                )
                .await
        })
    };

    context.await_sleep_started().await;
    registry
        .append_event(
            &ProcessId::from(process_id),
            lash_core::ProcessEventAppendRequest::cancel_requested(&registry.resolve_process_ref(&ProcessId::from(process_id)).await.expect("retained cancellation target"),
&lash_core::CancelRequest::new(lash_core::CancelOrigin::OperatorRequested, "actor:fixture:process_sleep_wake_verdict_failure_retries_before_settling_recorded_cancel", 11)),
        )
        .await
        .expect("commit cancel before wake");
    context.commit_process_cancel();
    context.fail_next_process_cancel_peeks(1);
    context.release_sleep();

    let first_error = tokio::time::timeout(Duration::from_secs(5), first_run)
        .await
        .expect("first attempt must leave the guest after the wake-verdict failure")
        .expect("join first sleeping process attempt")
        .expect_err("wake-verdict failure must abort the handler instead of settling the process");
    let first_error_debug = format!("{first_error:?}");
    assert!(
        first_error_debug.contains("Retryable")
            && first_error_debug.contains("simulated transient wake-verdict peek failure"),
        "wake-boundary verdict failure must request Restate redelivery: {first_error_debug}"
    );
    assert_eq!(
        registry
            .get_process(&ProcessId::from(process_id))
            .await
            .expect("read process after retryable failure")
            .expect("sleeping process remains registered")
            .status,
        lash_core::ProcessStatus::Running,
        "retryable wake-verdict failure must not terminalize the process"
    );

    // The failed attempt never journaled a verdict, so its redrive replays the
    // recorded prefix and emits the wake verdict as a journal extension.
    context.start_replay_allowing_journal_extension();
    let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let retry = tokio::time::timeout(
        Duration::from_secs(5),
        workflow.run_registration(
            registration,
            ProcessExecutionContext::default()
                .with_execution_write_authority(execution_write_authority),
            controller
                .scoped_effect_controller(durable_admission(&ExecutionScope::process(process_id)))
                .expect("sleeping process retry scope"),
            0,
            None,
            pending_process_cancel_signal(),
        ),
    )
    .await
    .expect("redelivery must not park on the already completed sleep")
    .expect("redeliver sleeping process after transient registry failure");
    assert!(
        matches!(
            retry,
            lash_core::ProcessRunOutcome::Terminal { ref output, .. }
                if output.terminal_status() == Some(lash_core::ProcessStatus::Cancelled)
        ),
        "redelivered process must settle the committed cancellation: {retry:#?}"
    );
    assert_eq!(
        registry
            .get_process(&ProcessId::from(process_id))
            .await
            .expect("read retried process")
            .expect("retried process remains registered")
            .status,
        lash_core::ProcessStatus::Cancelled
    );
}

#[tokio::test]
pub(super) async fn process_sleep_wake_cancel_gap_preempts_replay_of_post_wake_effect() {
    let process_id = "sleep-cancel-post-wake-effect";
    let (registry, continuations) = process_stores();
    let registration = sleeping_then_tool_process_registration(&ProcessId::from(process_id)).await;
    registry
        .register_process(registration.clone())
        .await
        .expect("register sleeping post-wake-effect process");
    let worker = recovery_worker_with_plugins(
        Arc::clone(&registry),
        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
        vec![snapshot_recovery_tool_factory()],
    );
    let workflow = Arc::new(LashProcessWorkflowImpl::new_for_test(
        Arc::new(RestateCoreProcessRunner::new(worker)),
        Arc::clone(&registry),
        continuations,
    ));
    let context = Arc::new(ReplayableRecordingContext::default());
    context.park_sleeps();
    context.crash_after_next_run_commit();
    let execution_write_authority = lash_core::ProcessExecutionWriteAuthority::invocation(
        process_id,
        "sleep-cancel-post-wake-effect-invocation",
    );
    let first_run = {
        let workflow = Arc::clone(&workflow);
        let context = Arc::clone(&context);
        let registration = registration.clone();
        let execution_write_authority = execution_write_authority.clone();
        tokio::spawn(async move {
            let controller = RestateRuntimeEffectController::new_for_test(context);
            workflow
                .run_registration(
                    registration,
                    ProcessExecutionContext::default()
                        .with_execution_write_authority(execution_write_authority),
                    controller
                        .scoped_effect_controller(durable_admission(&ExecutionScope::process(
                            process_id,
                        )))
                        .expect("sleeping post-wake-effect scope"),
                    0,
                    None,
                    pending_process_cancel_signal(),
                )
                .await
        })
    };

    context.await_sleep_started().await;
    context.release_sleep();
    context.await_run_committed().await;

    let crash = first_run
        .await
        .expect_err("injected worker failure must crash the first attempt");
    assert!(crash.is_panic(), "unexpected first-attempt exit: {crash}");

    let recorded_before_crash = context.recorded_runtime_effect_envelopes();
    assert_eq!(
        recorded_before_crash.len(),
        1,
        "exactly one post-wake effect must commit before the injected crash"
    );
    assert!(
        recorded_before_crash.iter().all(|(_, envelope)| matches!(
            envelope.command,
            RuntimeEffectCommand::ToolAttempt { .. }
        )),
        "the committed post-wake suffix must be the tool attempt: {recorded_before_crash:#?}"
    );
    assert_eq!(
        context.sleeps.lock_recover().as_slice(),
        &[300_000],
        "the first attempt must cross exactly one sleep wake boundary"
    );

    assert_eq!(
        registry
            .get_process(&ProcessId::from(process_id))
            .await
            .expect("read process after worker crash")
            .expect("crashed process remains registered")
            .status,
        lash_core::ProcessStatus::Running,
        "the crash must land before process settlement"
    );

    registry
        .append_event(
            &ProcessId::from(process_id),
            lash_core::ProcessEventAppendRequest::cancel_requested(&registry.resolve_process_ref(&ProcessId::from(process_id)).await.expect("retained cancellation target"),
&lash_core::CancelRequest::new(lash_core::CancelOrigin::OperatorRequested, "actor:fixture:process_sleep_wake_cancel_gap_preempts_replay_of_post_wake_effect", 11)),
        )
        .await
        .expect("commit cancellation in the redelivery gap");
    let runs_before_redelivery = context.runs();
    context.start_replay();

    let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let redelivery = workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default()
                .with_execution_write_authority(execution_write_authority),
            controller
                .scoped_effect_controller(durable_admission(&ExecutionScope::process(process_id)))
                .expect("sleeping post-wake-effect redelivery scope"),
            0,
            None,
            async { Ok(()) },
        )
        .await
        .expect("ready cancellation must pre-empt guest replay");

    assert!(
        matches!(
            redelivery,
            lash_core::ProcessRunOutcome::Terminal { ref output, .. }
                if output.terminal_status() == Some(lash_core::ProcessStatus::Cancelled)
        ),
        "redelivery must settle cancellation instead of replay mismatch: {redelivery:#?}"
    );
    assert_eq!(
        context.sleeps.lock_recover().as_slice(),
        &[300_000],
        "the biased cancellation branch must win before guest sleep replay"
    );
    assert_eq!(
        context.runs(),
        runs_before_redelivery,
        "the recorded post-wake effect must not be replayed after cancellation wins"
    );
    assert_eq!(
        registry
            .get_process(&ProcessId::from(process_id))
            .await
            .expect("read redelivered process")
            .expect("redelivered process remains registered")
            .status,
        lash_core::ProcessStatus::Cancelled
    );
}

#[tokio::test]
pub(super) async fn typescript_artifact_runs_through_process_engine_to_terminal() {
    let registry = process_registry();
    registry
        .register_process(
            typescript_process_registration(&ProcessId::from("typescript-worker")).await,
        )
        .await
        .expect("register TypeScript process");

    let worker = recovery_worker(
        Arc::clone(&registry),
        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
    );
    let _ = worker
        .drive_pending_processes()
        .await
        .expect("run stored TypeScript artifact");
    assert_eq!(
        lash_core::NativeProcessWork::for_registry(Arc::clone(&registry))
            .await_terminal(&ProcessId::from("typescript-worker"))
            .await
            .expect("await TypeScript process"),
        process_success(serde_json::json!({ "ok": true }))
    );
}

pub(super) fn assert_lashlang_engine_record(
    record: &lash_core::ProcessRecord,
    expected_process_name: &str,
    expected_args: serde_json::Map<String, serde_json::Value>,
) {
    let ProcessInput::Engine { kind, payload } = record.input.as_ref() else {
        panic!(
            "persisted Lashlang process must use generic engine input, got {:?}",
            record.input
        );
    };
    assert_eq!(
        kind,
        lash_lashlang_runtime::LASHLANG_ENGINE_KIND,
        "persisted row must dispatch through the registered Lashlang process engine"
    );
    let decoded = lash_lashlang_runtime::LashlangProcessInput::from_payload(payload.clone())
        .expect("persisted Lashlang engine payload must decode after registry reopen");
    assert_eq!(decoded.process_name, expected_process_name);
    assert_eq!(decoded.args, expected_args);
}

/// Phase-B recovery: a TRIGGER-started process whose worker died mid-flight is
/// left non-terminal in the durable registry; a subsequent worker reopening
/// that registry must drive it to completion via the recovery sweep — the same
/// durable re-execution guarantee a turn-started process has (invariant 3).
///
/// Mirrors `sqlite_process_recovery_reopens_registry_worker_observers_wakes_and_cancel`
/// but the process is started by a trigger occurrence (a `lashlang` engine row
/// with trigger provenance), not by a live turn's tool call.
#[tokio::test]
pub(super) async fn sqlite_trigger_started_process_recovered_after_worker_registry_reopen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let process_db = temp.path().join("processes.db");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        temp.path().join("sessions"),
    )) as Arc<dyn lash_core::SessionStoreFactory>;

    // A worker started the trigger process and crashed before it could run:
    // the durable row exists and is non-terminal. We register it directly to
    // model exactly that mid-flight crash state.
    let registry_a = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("open registry"),
    ) as Arc<dyn ProcessRegistry>;
    registry_a
        .register_process(
            trigger_lashlang_registration(&ProcessId::from("trigger-notify"), "issue-42").await,
        )
        .await
        .expect("register trigger-started process");
    let persisted_before_rebuild = registry_a
        .get_process(&ProcessId::from("trigger-notify"))
        .await
        .expect("read process")
        .expect("persisted trigger-started process before recovery");
    assert!(
        !persisted_before_rebuild.is_terminal(),
        "freshly trigger-started process must be non-terminal before recovery"
    );
    drop(registry_a);

    // Reopen the registry and stand up a fresh worker over it: the crash
    // recovery counterpart. The recovery sweep submits the non-terminal process
    // by workflow key; Restate coalesces duplicates and the workflow writes the
    // terminal outcome.
    let registry_b = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &process_db,
            process_db.with_extension("sessions"),
        )
        .await
        .expect("reopen registry"),
    ) as Arc<dyn ProcessRegistry>;
    let reopened_record = registry_b
        .get_process(&ProcessId::from("trigger-notify"))
        .await
        .expect("read process")
        .expect("trigger-started process survives registry reopen");
    assert_lashlang_engine_record(
        &reopened_record,
        "notify",
        serde_json::Map::from_iter([("resource".to_string(), serde_json::json!("issue-42"))]),
    );
    assert_eq!(
        registry_b
            .list_non_terminal_page(
                std::num::NonZeroUsize::new(16).expect("non-zero test page size"),
                None,
            )
            .await
            .expect("list non-terminal after reopen")
            .records
            .iter()
            .map(|record| record.id.as_str())
            .collect::<Vec<_>>(),
        vec!["trigger-notify"],
        "the trigger-started process must be on the recovery worklist after reopen"
    );

    let worker_b = recovery_worker(Arc::clone(&registry_b), Arc::clone(&store_factory));
    let _ = worker_b
        .drive_pending_processes()
        .await
        .expect("recover non-terminal trigger-started process");

    assert_eq!(
        lash_core::NativeProcessWork::for_registry(Arc::clone(&registry_b))
            .await_terminal(&ProcessId::from("trigger-notify"))
            .await
            .expect("await recovered trigger-started process"),
        process_success(serde_json::json!({ "triggered": "issue-42" })),
        "the trigger-started process must run to its terminal value on recovery"
    );
    assert!(
        registry_b
            .list_non_terminal_page(
                std::num::NonZeroUsize::new(16).expect("non-zero test page size"),
                None,
            )
            .await
            .expect("list non-terminal after recovery")
            .records
            .is_empty(),
        "recovery must drive the trigger-started process to terminal"
    );

    // Idempotent by process_id: re-running the sweep over an already-terminal
    // process is a no-op and never double-executes it.
    let _ = worker_b
        .drive_pending_processes()
        .await
        .expect("second recovery sweep is idempotent");
    assert_eq!(
        lash_core::NativeProcessWork::for_registry(Arc::clone(&registry_b))
            .await_terminal(&ProcessId::from("trigger-notify"))
            .await
            .expect("await after idempotent re-sweep"),
        process_success(serde_json::json!({ "triggered": "issue-42" }))
    );
}

/// A process tool that counts executions in a shared atomic.
pub(super) struct CountingProcessTool {
    executions: Arc<AtomicUsize>,
}

impl CountingProcessTool {
    pub(super) fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:recovery_count",
            "recovery_count",
            "Increment a shared execution counter (a stand-in non-idempotent side effect).",
            serde_json::json!({
                "type": "object",
                "properties": { "line": { "type": "string" } },
                "required": ["line"],
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        )
        .with_tool_binding(ToolBinding::new(["tools"], "recovery_count"))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for CountingProcessTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "recovery_count").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            let executed = self.executions.fetch_add(1, Ordering::SeqCst) + 1;
            let line = call
                .args
                .get("line")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            lash_core::ToolOutcome::ok(serde_json::json!({ "executed": executed, "line": line }))
        })
        .await
        .into()
    }
}

pub(super) fn counting_tool_plugin(
    executions: Arc<AtomicUsize>,
) -> Arc<dyn lash_core::facade_support::PluginFactory> {
    Arc::new(lash_core::plugin::StaticPluginFactory::new(
        "counting-process-tool",
        lash_core::facade_support::PluginSpec::new()
            .with_tool_provider(Arc::new(CountingProcessTool { executions })),
    ))
}

pub(super) fn counting_tool_registration(
    id: &str,
    disposition: lash_core::RecoveryContract,
    env_ref: lash_core::ProcessExecutionEnvRef,
) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::ToolCall {
            call: lash_core::PreparedToolCall::from_parts(
                format!("{id}-call"),
                "tool:recovery_count",
                "recovery_count",
                serde_json::json!({ "line": id }),
                None,
                serde_json::Value::Null,
            ),
        },
        disposition,
        lash_core::ProcessProvenance::host(),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
    .with_execution_env_ref(Some(env_ref))
}

pub(super) fn discover_service<S: Discoverable>(_: &S) -> restate_sdk::discovery::Service {
    S::discover()
}

#[tokio::test]
pub(super) async fn restate_workflows_and_wait_index_bind_with_required_handlers() {
    let runner = Arc::new(RecordingRunner::default());
    let registry = process_registry();
    let service =
        LashProcessWorkflowImpl::new_for_test(runner, registry, continuation_store()).serve();
    let discovery = discover_service(&service);
    let wait_workflow = LashDurableWaitWorkflowImpl.serve();
    let wait_workflow_discovery = discover_service(&wait_workflow);
    let wait_index = LashDurableWaitIndexImpl.serve();
    let wait_index_discovery = discover_service(&wait_index);
    let endpoint = Endpoint::builder()
        .bind(service)
        .bind(wait_workflow)
        .bind(wait_index)
        .build();

    assert_eq!(discovery.name.to_string(), "LashProcessWorkflow");
    assert_eq!(
        discovery.ty.to_string(),
        restate_sdk::discovery::ServiceType::Workflow.to_string()
    );
    assert_eq!(discovery.handlers.len(), 6);

    let run = discovery
        .handlers
        .iter()
        .find(|handler| handler.name.to_string() == "run")
        .expect("run handler discovery");
    let cancel = discovery
        .handlers
        .iter()
        .find(|handler| handler.name.to_string() == "cancel")
        .expect("cancel handler discovery");
    let await_terminal = discovery
        .handlers
        .iter()
        .find(|handler| handler.name.to_string() == "await_terminal")
        .expect("await_terminal handler discovery");
    let complete_terminal = discovery
        .handlers
        .iter()
        .find(|handler| handler.name.to_string() == "complete_terminal")
        .expect("complete_terminal handler discovery");
    let deliver_cancel = discovery
        .handlers
        .iter()
        .find(|handler| handler.name.to_string() == "deliver_cancel")
        .expect("deliver_cancel handler discovery");
    let await_cancel = discovery
        .handlers
        .iter()
        .find(|handler| handler.name.to_string() == "await_cancel")
        .expect("await_cancel handler discovery");

    assert_eq!(
        run.ty.as_ref().map(ToString::to_string).as_deref(),
        Some("WORKFLOW")
    );
    assert_eq!(
        cancel.ty.as_ref().map(ToString::to_string).as_deref(),
        Some("SHARED")
    );
    assert_eq!(
        await_terminal
            .ty
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("SHARED")
    );
    assert_eq!(
        complete_terminal
            .ty
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("SHARED")
    );
    assert_eq!(
        deliver_cancel
            .ty
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("SHARED")
    );
    assert_eq!(
        await_cancel.ty.as_ref().map(ToString::to_string).as_deref(),
        Some("SHARED")
    );

    let response = endpoint.handle(
        http::Request::builder()
            .uri("/discover")
            .header("accept", "application/vnd.restate.endpointmanifest.v3+json")
            .body(Empty::<bytes::Bytes>::new())
            .expect("discover request"),
    );
    assert_eq!(response.status(), http::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/vnd.restate.endpointmanifest.v3+json")
    );
    let body = response
        .into_body()
        .collect()
        .await
        .expect("discover response body")
        .to_bytes();
    let manifest: serde_json::Value =
        serde_json::from_slice(&body).expect("discover response json");
    let workflow = manifest["services"]
        .as_array()
        .expect("services array")
        .iter()
        .find(|service| service["name"] == "LashProcessWorkflow")
        .expect("workflow service");
    let handlers = workflow["handlers"].as_array().expect("handlers array");
    assert!(
        handlers
            .iter()
            .any(|handler| handler["name"] == "run" && handler["ty"] == "WORKFLOW")
    );
    assert!(
        handlers
            .iter()
            .any(|handler| handler["name"] == "cancel" && handler["ty"] == "SHARED")
    );
    assert!(
        handlers
            .iter()
            .any(|handler| handler["name"] == "await_terminal" && handler["ty"] == "SHARED")
    );
    assert!(
        handlers
            .iter()
            .any(|handler| { handler["name"] == "complete_terminal" && handler["ty"] == "SHARED" })
    );
    assert!(
        handlers
            .iter()
            .any(|handler| { handler["name"] == "deliver_cancel" && handler["ty"] == "SHARED" })
    );
    assert!(
        handlers
            .iter()
            .any(|handler| { handler["name"] == "await_cancel" && handler["ty"] == "SHARED" })
    );
    assert_eq!(
        wait_workflow_discovery.name.to_string(),
        "LashDurableWaitWorkflow"
    );
    assert!(wait_workflow_discovery.handlers.iter().any(|handler| {
        handler.name.to_string() == "await_resolution"
            && handler.ty.as_ref().map(ToString::to_string).as_deref() == Some("SHARED")
    }));
    assert_eq!(wait_workflow_discovery.handlers.len(), 3);
    assert!(
        wait_workflow_discovery
            .handlers
            .iter()
            .all(|handler| handler.name.to_string() != "observe"),
        "the superseded observe handoff must not remain registered"
    );
    assert!(wait_workflow_discovery.handlers.iter().any(|handler| {
        handler.name.to_string() == "resolve"
            && handler.ty.as_ref().map(ToString::to_string).as_deref() == Some("SHARED")
    }));
    assert_eq!(
        wait_index_discovery.name.to_string(),
        "LashDurableWaitIndex"
    );
    for required in ["register", "settle", "resolve", "cancel_all", "revoke_all"] {
        assert!(
            wait_index_discovery
                .handlers
                .iter()
                .any(|handler| handler.name.to_string() == required),
            "missing wait-index handler {required}"
        );
    }
}

#[tokio::test]
pub(super) async fn process_deployment_driver_and_workflow_share_registry() {
    let registry = process_registry();
    let deployment = RestateProcessDeployment::new_for_test(
        "http://127.0.0.1:8080",
        Arc::clone(&registry),
        continuation_store(),
    );
    let process_work = deployment.process_work();

    let worker = DurableProcessWorker::new(lash_core_worker::DurableProcessWorkerConfig::new(
        Arc::new(lash_core::facade_support::PluginHost::empty()),
        lash_core::facade_support::RuntimeHostConfig::in_memory(
            lash_core::CommitBudget::bounded(1024 * 1024, 512),
            lash_core::QueuedWorkBatchingConfig::new(1),
        ),
        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
        lash_core_worker::WorkerProcessWork::External(process_work),
        Arc::new(lash_core::NoQueuedWork::new()),
        lash_core::testing::runtime_lease_owner(),
    ))
    .expect("valid test native substrate config");
    let service = deployment.workflow(worker).serve();
    let discovery = discover_service(&service);
    let endpoint = Endpoint::builder().bind(service).build();

    assert_eq!(discovery.name.to_string(), "LashProcessWorkflow");
    assert!(discovery.handlers.iter().any(|handler| {
        handler.name.to_string() == "run"
            && handler.ty.as_ref().map(ToString::to_string).as_deref() == Some("WORKFLOW")
    }));
    assert!(discovery.handlers.iter().any(|handler| {
        handler.name.to_string() == "cancel"
            && handler.ty.as_ref().map(ToString::to_string).as_deref() == Some("SHARED")
    }));
    assert!(discovery.handlers.iter().any(|handler| {
        handler.name.to_string() == "await_terminal"
            && handler.ty.as_ref().map(ToString::to_string).as_deref() == Some("SHARED")
    }));

    let response = endpoint.handle(
        http::Request::builder()
            .uri("/discover")
            .header("accept", "application/vnd.restate.endpointmanifest.v3+json")
            .body(Empty::<bytes::Bytes>::new())
            .expect("discover request"),
    );
    assert_eq!(response.status(), http::StatusCode::OK);
}

#[tokio::test]
pub(super) async fn process_workflow_impl_runs_and_cancels_through_runner() {
    let runner = Arc::new(RecordingRunner::default());
    let registry = process_registry();
    let workflow = LashProcessWorkflowImpl::new_for_test(
        runner.clone(),
        registry.clone(),
        continuation_store(),
    );
    // The workflow only ever runs lash-executed rows: `submit_record` refuses to
    // POST an ExternallyOwned row, and the registry rejects a workflow-key
    // completion of one (ADR 0027) — so the fixture is Rerunnable.
    let registration = rerunnable_registration("task-workflow")
        .with_wake_session_id(Some(SessionId::from("wake-session")));
    registry
        .register_process(registration.clone())
        .await
        .expect("register workflow process");
    let execution_context = ProcessExecutionContext::default().with_causal_invocation(Some(
        runtime_invocation(RuntimeEffectKind::ToolAttempt, "tool-effect").into_runtime_invocation(),
    ));

    let record = registry
        .get_process(&ProcessId::from("task-workflow"))
        .await
        .expect("read workflow target")
        .expect("retained target");
    assert!(!record.is_terminal());
    assert!(record.cancel_request.is_none());
    let cancel = RestateProcessCancelRequest {
        process_ref: lash_core::ProcessRef::from_record(&record),
        request: lash_core::CancelRequest::new(
            lash_core::CancelOrigin::OperatorRequested,
            "actor:workflow-test",
            11,
        ),
    };
    workflow
        .cancel_registration(cancel.clone())
        .await
        .expect("workflow cancel while target is nonterminal");

    let output = workflow
        .run_registration(
            registration,
            execution_context,
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default()),
                durable_admission(&ExecutionScope::process("task-workflow")),
            )
            .expect("native process scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect("workflow run");

    assert!(matches!(
        output,
        lash_core::ProcessRunOutcome::Terminal { output, .. }
            if is_process_success(output.as_ref())
    ));
    assert_eq!(
        runner.ran.lock_recover().as_slice(),
        &[RecordedProcessRun {
            process_id: ProcessId::from("task-workflow"),
            wake_target_session_id: Some(SessionId::from("wake-session")),
            tool_effect_id: Some("tool-effect".to_string()),
            execution_scope_id: "task-workflow".to_string(),
            effect_journaling: lash_core::EffectJournaling::Local,
        }]
    );
    assert_eq!(
        runner.cancelled.lock_recover().as_slice(),
        std::slice::from_ref(&cancel)
    );
}

#[tokio::test]
pub(super) async fn terminal_retry_returns_the_stored_outcome() {
    let runner = Arc::new(RecordingRunner::default());
    let registry = process_registry();
    let workflow =
        LashProcessWorkflowImpl::new_for_test(runner, registry.clone(), continuation_store());
    registry
        .register_process(rerunnable_registration("terminal-retry"))
        .await
        .expect("register process");
    let stored = process_success(serde_json::json!({"winner": "stored"}));
    registry
        .complete_process(
            &ProcessId::from("terminal-retry"),
            stored.clone(),
            lash_core::ProcessCompletionAuthority::workflow_key("terminal-retry"),
        )
        .await
        .expect("commit terminal");

    let replayed = workflow
        .complete_with_stored_outcome(
            &ProcessId::from("terminal-retry"),
            process_failure(
                lash_core::ToolFailureClass::Execution,
                "divergent",
                "must not replace the stored outcome",
                None,
            ),
        )
        .await
        .expect("terminal retry");

    assert_eq!(replayed, stored);
    assert_eq!(
        registry
            .full_event_window(&ProcessId::from("terminal-retry"), 0)
            .await
            .expect("terminal events")
            .into_iter()
            .filter(|event| event.semantics.terminal.is_some())
            .count(),
        1
    );
}

pub(super) fn invocation_started(
    process_id: &ProcessId,
    execution_id: &str,
    attempt: u32,
) -> (
    lash_core::ProcessExecutionWriteAuthority,
    lash_core::ProcessStarted,
) {
    let authority = lash_core::ProcessExecutionWriteAuthority::invocation(process_id, execution_id)
        .bind_attempt(attempt);
    let mut started = authority
        .invocation_started()
        .expect("bound invocation authority");
    started.started_at_ms = u64::from(attempt);
    (authority, started)
}

#[tokio::test]
pub(super) async fn restate_invocation_identity_distinguishes_replay_from_fresh_execution() {
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("invocation-rerun").with_max_attempts(Some(2)))
        .await
        .expect("register rerunnable");

    let (first_authority, first_started) =
        invocation_started(&ProcessId::from("invocation-rerun"), "invocation-1", 1);
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                &ProcessId::from("invocation-rerun"),
                first_started.clone(),
                &first_authority,
            )
            .await
            .expect("first invocation"),
        lash_core::ProcessStartOutcome::Started(_)
    ));
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                &ProcessId::from("invocation-rerun"),
                first_started,
                &first_authority,
            )
            .await
            .expect("cross-replica journal replay"),
        lash_core::ProcessStartOutcome::AlreadyApplied(_)
    ));

    let (second_authority, second_started) =
        invocation_started(&ProcessId::from("invocation-rerun"), "invocation-2", 2);
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                &ProcessId::from("invocation-rerun"),
                second_started,
                &second_authority,
            )
            .await
            .expect("fresh invocation"),
        lash_core::ProcessStartOutcome::Started(_)
    ));
    let (third_authority, third_started) =
        invocation_started(&ProcessId::from("invocation-rerun"), "invocation-3", 3);
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                &ProcessId::from("invocation-rerun"),
                third_started,
                &third_authority,
            )
            .await
            .expect("attempt budget verdict"),
        lash_core::ProcessStartOutcome::AttemptsExhausted {
            attempts: 2,
            max_attempts: 2,
            ..
        }
    ));
}

#[tokio::test]
pub(super) async fn segment_zero_ignores_stale_carried_execution_identity() {
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("root-stale-id").with_max_attempts(Some(2)))
        .await
        .expect("register rerunnable");
    let (first_authority, first_started) =
        invocation_started(&ProcessId::from("root-stale-id"), "stale-invocation", 1);
    registry
        .record_first_started_with_authority(
            &ProcessId::from("root-stale-id"),
            first_started.clone(),
            &first_authority,
        )
        .await
        .expect("record old attempt");

    let (execution_id, authority) = segment_execution_authority(
        &ProcessId::from("root-stale-id"),
        0,
        Some("stale-invocation"),
        "fresh-invocation",
        Some(&first_started),
    )
    .expect("segment-zero identity");
    assert_eq!(execution_id, "fresh-invocation");
    let authority = authority.bind_attempt(2);
    let mut started = authority
        .invocation_started()
        .expect("bound fresh invocation");
    started.started_at_ms = 2;
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                &ProcessId::from("root-stale-id"),
                started,
                &authority
            )
            .await
            .expect("fresh root attempt"),
        lash_core::ProcessStartOutcome::Started(_)
    ));
}

#[tokio::test]
pub(super) async fn redriven_mid_chain_segment_consumes_attempt_and_respects_budget() {
    let registry = process_registry();
    registry
        .register_process(
            owner_bound_registration("owner-bound-redrive").with_max_attempts(Some(2)),
        )
        .await
        .expect("register owner-bound");
    let (root_authority, root_started) = invocation_started(
        &ProcessId::from("owner-bound-redrive"),
        "root-invocation",
        1,
    );
    registry
        .record_first_started_with_authority(
            &ProcessId::from("owner-bound-redrive"),
            root_started.clone(),
            &root_authority,
        )
        .await
        .expect("record root");

    let (_, redrive_authority) = segment_execution_authority(
        &ProcessId::from("owner-bound-redrive"),
        1,
        None,
        "redrive-invocation-1",
        Some(&root_started),
    )
    .expect("validated handover redrive identity");
    let redrive_authority = redrive_authority.bind_attempt(2);
    let mut redrive_started = redrive_authority
        .invocation_started()
        .expect("bound redrive");
    redrive_started.started_at_ms = 2;
    let redrive_record = match registry
        .record_first_started_with_authority(
            &ProcessId::from("owner-bound-redrive"),
            redrive_started,
            &redrive_authority,
        )
        .await
        .expect("owner-bound continuation may rebind at a handover")
    {
        lash_core::ProcessStartOutcome::Started(record) => record,
        other => panic!("expected a new continuation attempt, got {other:?}"),
    };
    assert_eq!(
        redrive_record
            .first_started
            .as_deref()
            .map(|started| started.attempt),
        Some(2)
    );

    let retained = redrive_record
        .first_started
        .as_deref()
        .expect("retained redrive start");
    let (_, exhausted_authority) = segment_execution_authority(
        &ProcessId::from("owner-bound-redrive"),
        1,
        None,
        "redrive-invocation-2",
        Some(retained),
    )
    .expect("second handover redrive identity");
    let exhausted_authority = exhausted_authority.bind_attempt(3);
    let exhausted_started = exhausted_authority
        .invocation_started()
        .expect("bound exhausted redrive");
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                &ProcessId::from("owner-bound-redrive"),
                exhausted_started,
                &exhausted_authority,
            )
            .await
            .expect("attempt budget verdict"),
        lash_core::ProcessStartOutcome::AttemptsExhausted {
            attempts: 2,
            max_attempts: 2,
            ..
        }
    ));
}

#[tokio::test]
pub(super) async fn rerunnable_mid_chain_redrive_continues_from_validated_handover() {
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("rerunnable-redrive"))
        .await
        .expect("register rerunnable");
    let (root_authority, root_started) =
        invocation_started(&ProcessId::from("rerunnable-redrive"), "root-invocation", 1);
    registry
        .record_first_started_with_authority(
            &ProcessId::from("rerunnable-redrive"),
            root_started.clone(),
            &root_authority,
        )
        .await
        .expect("record root");

    let (_, redrive_authority) = segment_execution_authority(
        &ProcessId::from("rerunnable-redrive"),
        1,
        None,
        "redrive-invocation",
        Some(&root_started),
    )
    .expect("validated handover redrive identity");
    let redrive_authority = redrive_authority.bind_attempt(2);
    let redrive_started = redrive_authority
        .invocation_started()
        .expect("bound redrive");
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                &ProcessId::from("rerunnable-redrive"),
                redrive_started,
                &redrive_authority,
            )
            .await
            .expect("rerunnable continuation"),
        lash_core::ProcessStartOutcome::Started(_)
    ));
}

#[tokio::test]
pub(super) async fn owner_bound_segment_continuation_reuses_root_invocation_identity() {
    let registry = process_registry();
    registry
        .register_process(owner_bound_registration("owner-bound-segment"))
        .await
        .expect("register owner-bound");
    let (root_authority, root_started) = invocation_started(
        &ProcessId::from("owner-bound-segment"),
        "root-invocation",
        1,
    );
    registry
        .record_first_started_with_authority(
            &ProcessId::from("owner-bound-segment"),
            root_started.clone(),
            &root_authority,
        )
        .await
        .expect("start root segment");

    let (execution_id, successor_authority) = segment_execution_authority(
        &ProcessId::from("owner-bound-segment"),
        1,
        Some("root-invocation"),
        "successor-handler-invocation",
        Some(&root_started),
    )
    .expect("validated live successor");
    assert_eq!(execution_id, "root-invocation");
    let successor_authority = successor_authority.bind_attempt(1);
    let mut successor_started = successor_authority
        .invocation_started()
        .expect("bound successor");
    successor_started.started_at_ms = root_started.started_at_ms;
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                &ProcessId::from("owner-bound-segment"),
                successor_started,
                &successor_authority,
            )
            .await
            .expect("mid-chain continuation"),
        lash_core::ProcessStartOutcome::AlreadyApplied(_)
    ));
    let (fresh_authority, fresh_started) = invocation_started(
        &ProcessId::from("owner-bound-segment"),
        "fresh-invocation",
        2,
    );
    assert!(matches!(
        registry
            .record_first_started_with_authority(
                &ProcessId::from("owner-bound-segment"),
                fresh_started,
                &fresh_authority,
            )
            .await
            .expect("fresh owner-bound invocation verdict"),
        lash_core::ProcessStartOutcome::AlreadyStarted { .. }
    ));
}

#[tokio::test]
pub(super) async fn run_registration_abandons_restarted_owner_bound_without_running() {
    // When the engine re-invokes the workflow for an OwnerBound row whose prior
    // incarnation already recorded `first_started` but left no outcome, the run
    // handler must not re-execute it. The workflow-key recovery path records an
    // Abandoned{Sweep} terminal so durable awaiters resolve.
    let started_owner = lash_core::LeaseOwnerIdentity::opaque("owner-a", "incarnation-1");
    let runner = Arc::new(AlreadyStartedRunner {
        calls: Mutex::new(0),
        winner: started_owner.clone(),
    });
    let registry = process_registry();
    let workflow = LashProcessWorkflowImpl::new_for_test(
        runner.clone(),
        registry.clone(),
        continuation_store(),
    );
    let registration = owner_bound_registration("ob-restart");
    registry
        .register_process(registration.clone())
        .await
        .expect("register owner-bound process");
    // Simulate the prior incarnation that began executing but never completed.
    registry
        .record_first_started(
            &ProcessId::from("ob-restart"),
            lash_core::ProcessStarted {
                owner: started_owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 42,
                replay_grammar: None,
            },
        )
        .await
        .expect("record prior incarnation start");

    let output = workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default(),
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default()),
                durable_admission(&ExecutionScope::process("ob-restart")),
            )
            .expect("native process scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect("run_registration");

    // The real runner rejects this before user-code execution when its atomic
    // start write observes the prior OwnerBound attempt.
    assert_eq!(*runner.calls.lock_recover(), 1);
    let lash_core::ProcessRunOutcome::Terminal { output, .. } = &output else {
        panic!("expected terminal output, got {output:?}");
    };
    let ProcessAwaitOutput::Abandoned { evidence, .. } = output.as_ref() else {
        panic!("expected Abandoned output, got {output:?}");
    };
    assert_eq!(evidence.writer, AbandonWriter::Sweep);
    assert_eq!(evidence.owner.as_ref(), Some(&started_owner));
    let record = registry
        .get_process(&ProcessId::from("ob-restart"))
        .await
        .expect("read process")
        .expect("get abandoned row");
    assert!(record.is_terminal(), "the row is completed as terminal");
    assert!(matches!(
        record.outcome,
        Some(ProcessAwaitOutput::Abandoned { .. })
    ));
}

#[tokio::test]
pub(super) async fn run_registration_runs_fresh_owner_bound() {
    // A fresh OwnerBound row has no `first_started` (the runner records it inside
    // run_process, during execution), so the re-invocation guard must NOT fire:
    // the runner executes normally on the first invocation.
    let runner = Arc::new(RecordingRunner::default());
    let registry = process_registry();
    let workflow = LashProcessWorkflowImpl::new_for_test(
        runner.clone(),
        registry.clone(),
        continuation_store(),
    );
    let registration = owner_bound_registration("ob-fresh");
    registry
        .register_process(registration.clone())
        .await
        .expect("register fresh owner-bound process");

    let output = workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default(),
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default()),
                durable_admission(&ExecutionScope::process("ob-fresh")),
            )
            .expect("native process scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect("run_registration");

    assert!(matches!(
        output,
        lash_core::ProcessRunOutcome::Terminal { output, .. }
            if is_process_success(output.as_ref())
    ));
    assert_eq!(
        runner
            .ran
            .lock_recover()
            .iter()
            .map(|run| run.process_id.clone())
            .collect::<Vec<_>>(),
        vec!["ob-fresh".to_string()],
        "a fresh OwnerBound row runs through the runner on first invocation"
    );
}

/// FIG-2964: submission is keyed by segment and coalesces.
///
/// The first scan submits the segment-0 workflow key and records the external
/// reference that says Restate owns the row. A second scan re-reads the row,
/// sees that reference, and skips: resubmitting would be a second POST for a
/// run already in flight, and the workflow key is the only coalescing point.
#[tokio::test]
pub(super) async fn ingress_runner_submits_by_segment_key_once_and_coalesces_the_repeat_scan() {
    // A non-terminal, Lash-executed (Rerunnable) process is the durable
    // worklist row the ingress runner must submit. ExternallyOwned rows are
    // never submitted (ADR 0019), so the submittable case uses a Rerunnable row.
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("task-1"))
        .await
        .expect("register");

    // The capture server accepts exactly one connection, so a second submit
    // would have nothing to talk to: the single-response server is itself part
    // of the proof that the repeat scan does not POST.
    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "202 Accepted",
        body: r#"{"invocationId":"inv_task_1","status":"Accepted"}"#,
    }])
    .await;

    let runner = RestateProcessIngressRunner::new(base_url, registry.clone(), continuation_store());
    let first = runner
        .admit_pending_processes("test")
        .await
        .expect("drive pending");
    let second = runner
        .admit_pending_processes("test")
        .await
        .expect("drive pending again");
    server.await.expect("mock ingress server task");

    let requests = captured.lock_recover().clone();
    assert_eq!(
        requests.len(),
        1,
        "the row is submitted once; the second scan coalesces onto the recorded reference: {requests:?}"
    );
    let request = &requests[0];
    assert!(
        request.starts_with("POST /LashProcessWorkflow/task-1/run/send "),
        "submits the segment-0 workflow key: {request}"
    );
    assert!(
        !request.contains("idempotency-key:"),
        "workflow sends must not carry an idempotency header; Restate coalesces by workflow key: {request}"
    );
    assert_eq!(first.admitted, vec!["task-1".to_string()]);
    assert!(
        second.admitted.is_empty(),
        "a row Restate already owns is not admitted again: {second:?}"
    );
    assert_eq!(
        second
            .deferred
            .iter()
            .map(|entry| (entry.process_id.to_string(), entry.disposition.clone()))
            .collect::<Vec<_>>(),
        vec![("task-1".to_string(), ProcessRecoveryAttemptOutcome::Busy)],
        "the skip is a typed deferral, not a silent drop"
    );

    // The durable backend reference is recorded so the process is observably
    // owned by Restate, and it names the segment it was minted for.
    let record = registry
        .get_process(&ProcessId::from("task-1"))
        .await
        .expect("read process")
        .expect("get process");
    let external = record.external_ref.as_ref().expect("external ref recorded");
    assert_eq!(external.backend.as_str(), "restate");
    assert_eq!(external.id, "LashProcessWorkflow/task-1");
    assert_eq!(
        external.segment_ordinal,
        Some(0),
        "a live start always schedules the first segment"
    );
    assert_eq!(
        external
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("invocation_id")),
        Some(&serde_json::json!("inv_task_1"))
    );
}

/// FIG-2964 acceptance: recovery of a row that has handed over once keys the
/// submission by segment 1, not by the bare process id.
///
/// Segment 1 is the boundary case the keying scheme has to get right: it is the
/// first ordinal that is not the process id itself, so a scheme that only
/// special-cased "has a handover" would resubmit segment 0 and run the process
/// twice from the start.
///
/// The row reaches its state through a live boundary — a real start that
/// recorded its segment-0 reference, then a real handover whose successor send
/// never landed — because that is the state the sweep actually meets. A
/// hand-built handover with no reference at all is unreachable from any live
/// start, and testing against it would let a skip keyed on
/// `external_ref.is_some()` pass while stranding every real row.
#[tokio::test]
pub(super) async fn ingress_sweep_keys_segment_one_recovery_by_its_segment_workflow_key() {
    let (registry, continuations, _boundary) =
        super::restate_redrive::drive_to_live_segment_boundary("handed-over-once").await;

    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "202 Accepted",
        body: r#"{"invocationId":"inv_handed_over_1","status":"Accepted"}"#,
    }])
    .await;
    let runner =
        RestateProcessIngressRunner::new(base_url, Arc::clone(&registry), continuations.clone());
    let _ = runner
        .admit_pending_processes("test")
        .await
        .expect("drive pending");
    server.await.expect("mock ingress server task");

    let requests = captured.lock_recover().clone();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].starts_with("POST /LashProcessWorkflow/handed-over-once%231/run/send "),
        "segment-1 recovery addresses the segment-1 workflow key: {}",
        requests[0]
    );
    assert!(
        !requests[0].starts_with("POST /LashProcessWorkflow/handed-over-once/run/send "),
        "keying by the bare id would rerun the process from segment 0: {}",
        requests[0]
    );
    assert!(
        requests[0].contains("\"segment_ordinal\":1"),
        "the submitted input must carry the ordinal it was keyed for: {}",
        requests[0]
    );

    // The reference written for segment 1 names its ordinal, so a later
    // compare-and-set can tell it apart from a stale segment-0 reference.
    let record = registry
        .get_process(&ProcessId::from("handed-over-once"))
        .await
        .expect("read process")
        .expect("get process");
    let external = record.external_ref.as_ref().expect("external ref recorded");
    assert_eq!(external.id, "LashProcessWorkflow/handed-over-once#1");
    assert_eq!(external.segment_ordinal, Some(1));
}

/// FIG-2964 regression: the sweep's skip is keyed on the recorded reference's
/// *ordinal*, not on a reference merely existing.
///
/// Both rows below come from the same live boundary. The first has handed over
/// to segment 1 while its reference still names segment 0 — a crashed successor
/// Restate owns nothing for, so the sweep must submit it. The second has
/// completed its handover, so the reference names segment 1 and Restate does own
/// it: the sweep defers. Skipping on `external_ref.is_some()` would pass the
/// second and strand the first forever, and after this PR *every* handed-over
/// row carries a reference, so that skip would strand all of them.
#[tokio::test]
pub(super) async fn ingress_sweep_resubmits_a_stale_reference_and_defers_the_current_one() {
    let (registry, continuations, boundary) =
        super::restate_redrive::drive_to_live_segment_boundary("ordinal-aware-skip").await;

    // Stale reference (segment 0) against a segment-1 handover: submit it.
    let (base_url, captured, server) = spawn_restate_http_capture(vec![MockHttpResponse {
        status: "202 Accepted",
        body: r#"{"invocationId":"inv_ordinal_aware_1","status":"Accepted"}"#,
    }])
    .await;
    let runner =
        RestateProcessIngressRunner::new(base_url, Arc::clone(&registry), continuations.clone());
    let stale = runner
        .admit_pending_processes("test")
        .await
        .expect("drive pending");
    server.await.expect("mock ingress server task");
    let requests = captured.lock_recover().clone();
    assert_eq!(
        requests.len(),
        1,
        "a reference one segment behind the handover must be resubmitted: {requests:?}"
    );
    assert!(
        requests[0].starts_with("POST /LashProcessWorkflow/ordinal-aware-skip%231/run/send "),
        "the resubmission addresses the latest segment, not the recorded one: {}",
        requests[0]
    );
    assert_eq!(
        stale.admitted,
        vec!["ordinal-aware-skip".to_string()],
        "the stale-reference row is admitted, not deferred"
    );

    // Now let the live handover complete, so the reference names segment 1.
    boundary.complete_handover("ordinal-aware-skip").await;
    let (base_url, captured, server) = spawn_restate_http_capture(vec![]).await;
    let runner =
        RestateProcessIngressRunner::new(base_url, Arc::clone(&registry), continuations.clone());
    let current = runner
        .admit_pending_processes("test")
        .await
        .expect("drive pending");
    server.await.expect("mock ingress server task");
    assert!(
        captured.lock_recover().is_empty(),
        "a row whose reference already names the latest segment is not resubmitted"
    );
    assert!(current.admitted.is_empty());
    assert_eq!(
        current
            .deferred
            .iter()
            .map(|entry| (entry.process_id.to_string(), entry.disposition.clone()))
            .collect::<Vec<_>>(),
        vec![(
            "ordinal-aware-skip".to_string(),
            ProcessRecoveryAttemptOutcome::Busy
        )],
        "the skip is a typed deferral, not a silent drop"
    );
}

/// FIG-2964: a host crash between registration and submission leaves exactly
/// the row the sweep is meant to own, and the sweep starts it exactly once.
///
/// A row carrying a standing cancel request is submitted too. Only a
/// `StartFailed` request is terminal on the spot; every other origin is
/// recorded and waits for a run to honour it, and the sweep never writes a
/// terminal of its own. Withholding the submission would therefore leave the
/// row permanently non-terminal — `await_process_terminal` would never return
/// and retention would never reclaim it. The run settles it instead
/// (`fig779_suspended_process_redrive_observes_durable_cancellation` pins that
/// a redriven segment observes its durable cancellation and terminalises).
#[tokio::test]
pub(super) async fn ingress_sweep_starts_the_crashed_row_once_and_submits_the_cancelling_row() {
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("crashed-before-submit"))
        .await
        .expect("register the row whose host died before it submitted");
    let cancelling = registry
        .register_process(rerunnable_registration("cancel-requested"))
        .await
        .expect("register the row that carries a standing cancel request");
    registry
        .request_process_cancel(
            &lash_core::ProcessRef::from_record(&cancelling),
            lash_core::CancelOrigin::OperatorRequested,
            "operator".to_string(),
            None,
        )
        .await
        .expect("record the standing cancel request");

    // Two responses, two accepted connections: a third submit would have
    // nothing to talk to, so the server shape is part of the "exactly once per
    // row" proof.
    let (base_url, captured, server) = spawn_restate_http_capture(vec![
        MockHttpResponse {
            status: "202 Accepted",
            body: r#"{"invocationId":"inv_crashed","status":"Accepted"}"#,
        },
        MockHttpResponse {
            status: "202 Accepted",
            body: r#"{"invocationId":"inv_cancelling","status":"Accepted"}"#,
        },
    ])
    .await;
    let runner =
        RestateProcessIngressRunner::new(base_url, Arc::clone(&registry), continuation_store());
    let report = runner
        .admit_pending_processes("test")
        .await
        .expect("sweep starts both rows");
    server.await.expect("mock ingress server task");

    let requests = captured.lock_recover().clone();
    assert_eq!(
        requests.len(),
        2,
        "each row is started exactly once: {requests:?}"
    );
    let mut submitted = requests
        .iter()
        .filter_map(|request| {
            request
                .strip_prefix("POST /LashProcessWorkflow/")
                .and_then(|rest| rest.split("/run/send").next())
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    submitted.sort();
    assert_eq!(
        submitted,
        vec![
            "cancel-requested".to_string(),
            "crashed-before-submit".to_string()
        ],
        "both rows reach the ingress, keyed by their own segment: {requests:?}"
    );
    let mut admitted = report
        .admitted
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    admitted.sort();
    assert_eq!(
        admitted,
        vec![
            "cancel-requested".to_string(),
            "crashed-before-submit".to_string()
        ]
    );
    assert!(
        report.deferred.is_empty(),
        "neither row is deferred: {:?}",
        report.deferred
    );

    // The cancel-requested row is now owned by Restate and still non-terminal:
    // the sweep submitted it and wrote no terminal of its own. Its run honours
    // the standing request.
    let cancelling = registry
        .get_process(&ProcessId::from("cancel-requested"))
        .await
        .expect("read process")
        .expect("the cancelling row stands");
    assert!(
        !cancelling.is_terminal(),
        "the sweep must never terminalise a row it did not run, got {:?}",
        cancelling.status
    );
    assert_eq!(
        cancelling
            .external_ref
            .as_ref()
            .map(|external| external.id.as_str()),
        Some("LashProcessWorkflow/cancel-requested"),
        "the submitted row names the workflow that will settle it"
    );
    assert!(cancelling.cancel_request.is_some());
}
