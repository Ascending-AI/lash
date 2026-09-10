use super::*;

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

#[tokio::test]
pub(super) async fn sqlite_process_recovery_terminalizes_revoked_snapshot_plugin_options() {
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
    registry_a
        .register_process(
            snapshot_lashlang_registration(&ProcessId::from("snapshot-revoked"), env_ref).await,
        )
        .await
        .expect("register revoked snapshot-backed process");
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
        .expect("recover revoked snapshot-backed process");

    let await_output = lash_core::NativeProcessWork::for_registry(Arc::clone(&registry_b))
        .await_terminal(&ProcessId::from("snapshot-revoked"))
        .await
        .expect("await terminal revoked snapshot-backed process");
    let ProcessAwaitOutput::Settled { output } = await_output else {
        panic!("expected revoked snapshot process failure, got {await_output:#?}");
    };
    let lash_core::ToolCallOutcome::Failure(failure) = output.outcome else {
        panic!("expected revoked snapshot process failure, got {output:#?}");
    };
    assert_eq!(failure.code, "process_host_environment_incompatible");
    assert!(
        failure
            .message
            .contains("module `tools` does not expose operation `snapshot_echo`"),
        "{}",
        failure.message
    );
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
    let module =
        lashlang::parse("process notify(resource: str) { finish { triggered: resource } }")
            .expect("lashlang trigger module");
    let linked_module = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::new(),
            lashlang::LashlangAbilities::all(),
        ),
    )
    .expect("link lashlang trigger module");
    lashlang::LashlangArtifactStore::put_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
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
            module_ref: linked_module.module_ref,
            process_ref,
            host_requirements_ref: linked_module.host_requirements_ref,
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
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(env_ref))
}

pub(super) async fn typescript_process_registration(process_id: &ProcessId) -> ProcessRegistration {
    let linked = lash_typescript::link(
        r#"
        const worker = defineProcess({
          name: "worker",
          signals: {},
          run: async () => { return { ok: true }; }
        });
        finish(null);
        "#,
        &lashlang::LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::new(),
            lashlang::LashlangAbilities::all(),
        ),
    )
    .expect("link TypeScript process");
    lashlang::LashlangArtifactStore::put_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
        &linked.artifact,
    )
    .await
    .expect("store TypeScript artifact");
    assert_eq!(
        linked.artifact.compilation_dialect,
        lashlang::CompilationDialect::Typescript
    );
    let process = linked
        .artifact
        .canonical_ir
        .process("worker")
        .expect("worker process declaration");
    let env_ref = persist_recovery_env_ref().await;
    ProcessRegistration::new(
        process_id,
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked.module_ref,
            process_ref: linked
                .artifact
                .process_ref("worker")
                .expect("worker process ref")
                .clone(),
            host_requirements_ref: linked.host_requirements_ref,
            process_name: "worker".to_string(),
            args: serde_json::Map::new(),
        }),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::host(),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_signal_event_types(
        process,
    ))
    .with_execution_env_ref(Some(env_ref))
}

pub(super) async fn sleeping_process_registration(
    process_id: &ProcessId,
    dialect: lashlang::CompilationDialect,
) -> ProcessRegistration {
    let environment = lashlang::LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::new(),
        lashlang::LashlangAbilities::all(),
    );
    let linked = match dialect {
        lashlang::CompilationDialect::Lashlang => lashlang::LinkedModule::link(
            lashlang::parse(
                r#"
                process worker() {
                  sleep for "5m"
                  finish "completed after wake"
                }
                "#,
            )
            .expect("parse sleeping Lashlang process"),
            environment,
        )
        .expect("link sleeping Lashlang process"),
        lashlang::CompilationDialect::Typescript => lash_typescript::link(
            r#"
            const worker = defineProcess({
              name: "worker",
              signals: {},
              run: async () => {
                await sleep(300000);
                return "completed after wake";
              }
            });
            finish(null);
            "#,
            &environment,
        )
        .expect("link sleeping TypeScript process"),
    };
    assert_eq!(linked.artifact.compilation_dialect, dialect);
    lashlang::LashlangArtifactStore::put_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
        &linked.artifact,
    )
    .await
    .expect("store sleeping process artifact");
    let env_ref = persist_recovery_env_ref().await;
    ProcessRegistration::new(
        process_id,
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked.module_ref,
            process_ref: linked
                .artifact
                .process_ref("worker")
                .expect("worker process ref")
                .clone(),
            host_requirements_ref: linked.host_requirements_ref,
            process_name: "worker".to_string(),
            args: serde_json::Map::new(),
        }),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::host(),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(env_ref))
}

pub(super) async fn sleeping_then_tool_process_registration(
    process_id: &ProcessId,
) -> ProcessRegistration {
    let module = lashlang::parse(
        r#"
        process worker() {
          sleep for "5m"
          called = await tools.snapshot_echo({ line: "after wake" })?
          finish called.echo
        }
        "#,
    )
    .expect("parse sleeping post-wake-effect process");
    let contract = SnapshotRecoveryTool::definition().contract();
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_module_operation(
            ["tools"],
            "Tools",
            "snapshot_echo",
            "tool:snapshot_echo",
            lashlang::json_schema_to_type_expr(contract.input_schema.canonical()),
            lashlang::json_schema_to_type_expr(contract.output_schema.canonical()),
        )
        .expect("link post-wake tool operation");
    let linked = lashlang::LinkedModule::link(
        module,
        lashlang::LashlangHostEnvironment::new(
            resources,
            lashlang::LashlangAbilities::default()
                .with_processes()
                .with_sleep(),
        ),
    )
    .expect("link sleeping post-wake-effect process");
    lashlang::LashlangArtifactStore::put_module_artifact(
        lashlang::global_in_memory_lashlang_artifact_store().as_ref(),
        &linked.artifact,
    )
    .await
    .expect("store sleeping post-wake-effect artifact");
    let env_ref = persist_snapshot_recovery_env_ref("tool-authority:sha256:ok").await;
    ProcessRegistration::new(
        process_id,
        lashlang_process_input(lash_lashlang_runtime::LashlangProcessInput {
            module_ref: linked.module_ref,
            process_ref: linked
                .artifact
                .process_ref("worker")
                .expect("worker process ref")
                .clone(),
            host_requirements_ref: linked.host_requirements_ref,
            process_name: "worker".to_string(),
            args: serde_json::Map::new(),
        }),
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::host(),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
    .with_execution_env_ref(Some(env_ref))
}

#[tokio::test]
pub(super) async fn process_sleep_wake_settles_recorded_cancel_before_resuming_either_dialect() {
    for dialect in [
        lashlang::CompilationDialect::Lashlang,
        lashlang::CompilationDialect::Typescript,
    ] {
        let dialect_label = match dialect {
            lashlang::CompilationDialect::Lashlang => "lashlang",
            lashlang::CompilationDialect::Typescript => "typescript",
        };
        let process_id = ProcessId::from(format!("sleep-cancel-{dialect_label}"));
        let (registry, continuations) = process_stores();
        let registration = sleeping_process_registration(&process_id, dialect).await;
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
        let execution_id = format!("sleep-cancel-invocation-{dialect_label}");
        let execution_write_authority =
            lash_core::ProcessExecutionWriteAuthority::invocation(&process_id, &execution_id);
        let run = {
            let workflow = Arc::clone(&workflow);
            let context = Arc::clone(&context);
            let process_id = process_id.clone();
            tokio::spawn(async move {
                let controller = RestateRuntimeEffectController::new(context);
                workflow
                    .run_registration(
                        registration,
                        ProcessExecutionContext::default()
                            .with_execution_write_authority(execution_write_authority),
                        controller
                            .scoped_effect_controller(ExecutionScope::process(&process_id))
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
                    &process_id,
                    Some("operator stopped sleeping process".to_string()),
                ),
            )
            .await
            .expect("commit cancel before wake");
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
            "{} process resumed past its wake and produced {outcome:#?}",
            dialect_label
        );
        assert_eq!(
            registry
                .get_process(&process_id)
                .await
                .expect("read sleeping process")
                .expect("sleeping process remains registered")
                .status,
            lash_core::ProcessStatus::Cancelled,
            "{} process terminal status",
            dialect_label
        );
    }
}

#[tokio::test]
pub(super) async fn process_sleep_wake_registry_failure_retries_before_settling_recorded_cancel() {
    let process_id = "sleep-cancel-read-retry";
    let storage = Arc::new(lash_core::TestLocalProcessRegistry::default());
    let registry = Arc::clone(&storage) as Arc<dyn ProcessRegistry>;
    let continuations = Arc::clone(&storage) as Arc<dyn lash_core::ProcessContinuationStore>;
    let registration = sleeping_process_registration(
        &ProcessId::from(process_id),
        lashlang::CompilationDialect::Lashlang,
    )
    .await;
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
            let controller = RestateRuntimeEffectController::new(context);
            workflow
                .run_registration(
                    registration,
                    ProcessExecutionContext::default()
                        .with_execution_write_authority(execution_write_authority),
                    controller
                        .scoped_effect_controller(ExecutionScope::process(process_id))
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
            lash_core::ProcessEventAppendRequest::cancel_requested(
                &ProcessId::from(process_id),
                Some("operator stopped sleeping process".to_string()),
            ),
        )
        .await
        .expect("commit cancel before wake");
    storage
        .set_process_events_read_error_for_testing(lash_core::PluginError::Runtime(
            lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::RuntimeStore,
                "simulated transient wake-boundary registry failure",
            ),
        ))
        .await;
    context.release_sleep();

    let first_error = tokio::time::timeout(Duration::from_secs(5), first_run)
        .await
        .expect("first attempt must leave the guest after the registry failure")
        .expect("join first sleeping process attempt")
        .expect_err("registry failure must abort the handler instead of settling the process");
    let first_error_debug = format!("{first_error:?}");
    assert!(
        first_error_debug.contains("Retryable")
            && first_error_debug.contains("simulated transient wake-boundary registry failure"),
        "wake-boundary registry failure must request Restate redelivery: {first_error_debug}"
    );
    assert_eq!(
        registry
            .get_process(&ProcessId::from(process_id))
            .await
            .expect("read process after retryable failure")
            .expect("sleeping process remains registered")
            .status,
        lash_core::ProcessStatus::Running,
        "retryable registry failure must not terminalize the process"
    );

    context.start_replay();
    let controller = RestateRuntimeEffectController::new(Arc::clone(&context));
    let retry = tokio::time::timeout(
        Duration::from_secs(5),
        workflow.run_registration(
            registration,
            ProcessExecutionContext::default()
                .with_execution_write_authority(execution_write_authority),
            controller
                .scoped_effect_controller(ExecutionScope::process(process_id))
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
            let controller = RestateRuntimeEffectController::new(context);
            workflow
                .run_registration(
                    registration,
                    ProcessExecutionContext::default()
                        .with_execution_write_authority(execution_write_authority),
                    controller
                        .scoped_effect_controller(ExecutionScope::process(process_id))
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
            lash_core::ProcessEventAppendRequest::cancel_requested(
                &ProcessId::from(process_id),
                Some("operator cancelled during the redelivery gap".to_string()),
            ),
        )
        .await
        .expect("commit cancellation in the redelivery gap");
    let runs_before_redelivery = context.runs();
    context.start_replay();

    let controller = RestateRuntimeEffectController::new(Arc::clone(&context));
    let redelivery = workflow
        .run_registration(
            registration,
            ProcessExecutionContext::default()
                .with_execution_write_authority(execution_write_authority),
            controller
                .scoped_effect_controller(ExecutionScope::process(process_id))
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
    fn definition() -> lash_core::ToolDefinition {
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

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        let executed = self.executions.fetch_add(1, Ordering::SeqCst) + 1;
        let line = call
            .args
            .get("line")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        lash_core::ToolOutcome::ok(serde_json::json!({ "executed": executed, "line": line }))
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
    let deployment = RestateProcessDeployment::new(
        "http://127.0.0.1:8080",
        Arc::clone(&registry),
        continuation_store(),
    );
    let process_work = deployment.process_work();

    let worker =
        DurableProcessWorker::new(lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::empty()),
            lash_core::facade_support::RuntimeHostConfig::in_memory(
                lash_core::CommitBudget::bounded(1024 * 1024, 512),
                lash_core::QueuedWorkBatchingConfig::new(1),
            ),
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            lash_core::WorkerProcessWork::External(process_work),
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
        runtime_invocation(RuntimeEffectKind::ToolAttempt, "tool-effect"),
    ));

    let output = workflow
        .run_registration(
            registration,
            execution_context,
            lash_core::ScopedEffectController::shared(
                Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default()),
                lash_core::ExecutionScope::process("task-workflow"),
            )
            .expect("native process scope"),
            0,
            None,
            pending_process_cancel_signal(),
        )
        .await
        .expect("workflow run");
    workflow
        .cancel_registration(RestateProcessCancelRequest {
            process_id: ProcessId::from("task-workflow"),
            reason: Some("stop".to_string()),
        })
        .await
        .expect("workflow cancel");

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
            turn_control_participation: lash_core::TurnControlParticipation::Local,
        }]
    );
    assert_eq!(
        runner.cancelled.lock_recover().as_slice(),
        &[RestateProcessCancelRequest {
            process_id: ProcessId::from("task-workflow"),
            reason: Some("stop".to_string()),
        }]
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
            .events_after(&ProcessId::from("terminal-retry"), 0)
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
                lash_core::ExecutionScope::process("ob-restart"),
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
                lash_core::ExecutionScope::process("ob-fresh"),
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

#[tokio::test]
pub(super) async fn ingress_runner_submits_non_terminal_process_by_workflow_key() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // A non-terminal, Lash-executed (Rerunnable) process is the durable
    // worklist row the ingress runner must submit. ExternallyOwned rows are
    // never submitted (ADR 0019), so the submittable case uses a Rerunnable row.
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration("task-1"))
        .await
        .expect("register");

    // Minimal mock ingress: capture two submissions, then reply 202 Accepted
    // so the reqwest submit succeeds. The second submit exercises the
    // registry's exact-repeat external_ref path for a still-running process.
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured_server = captured.clone();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.expect("read request");
            captured_server
                .lock_recover()
                .push(String::from_utf8_lossy(&buf[..n]).into_owned());
            socket
                .write_all(
                    b"HTTP/1.1 202 Accepted\r\ncontent-type: application/json\r\ncontent-length: 49\r\n\r\n{\"invocationId\":\"inv_task_1\",\"status\":\"Accepted\"}",
                )
                .await
                .expect("write response");
            socket.flush().await.expect("flush");
        }
    });

    let runner = RestateProcessIngressRunner::new(
        format!("http://{addr}"),
        registry.clone(),
        continuation_store(),
    );
    let _ = runner
        .admit_pending_processes("test")
        .await
        .expect("drive pending");
    let _ = runner
        .admit_pending_processes("test")
        .await
        .expect("drive pending again");
    server.await.expect("mock ingress server task");

    let requests = captured.lock_recover().clone();
    assert_eq!(
        requests.len(),
        2,
        "the non-terminal process must be submitted on both scans"
    );
    let request = &requests[0];
    assert!(
        request.starts_with("POST /LashProcessWorkflow/task-1/run/send "),
        "submits the keyed workflow run: {request}"
    );
    assert!(
        !request.contains("idempotency-key:"),
        "workflow sends must not carry an idempotency header; Restate coalesces by workflow key: {request}"
    );
    assert!(
        requests[1].starts_with("POST /LashProcessWorkflow/task-1/run/send "),
        "repeat scan submits the same keyed workflow run: {}",
        requests[1]
    );

    // The durable backend reference is recorded so the process is observably
    // owned by Restate.
    let record = registry
        .get_process(&ProcessId::from("task-1"))
        .await
        .expect("read process")
        .expect("get process");
    assert_eq!(
        record.external_ref.as_ref().map(|e| e.backend.as_str()),
        Some("restate"),
        "the durable external_ref must be recorded after a successful submit"
    );
    assert_eq!(
        record
            .external_ref
            .as_ref()
            .and_then(|external| external.metadata.as_ref())
            .and_then(|metadata| metadata.get("invocation_id")),
        Some(&serde_json::json!("inv_task_1"))
    );
}
