use super::*;

#[expect(clippy::expect_used, reason = "process host law fixture")]
pub(crate) async fn process_event_host_failure_stops_execution(
    backend: &lash_core::Backend,
    scoped: lash_core::ActorContext,
) {
    use lash_core::core_internal::RuntimeExecutionContextRuntimeOps as _;
    let registration = lash_core::ProcessRegistration::new(
        lash_core::ProcessInput::Engine {
            kind: LASHLANG_ENGINE_KIND.into(),
            payload: serde_json::json!({}),
        },
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    );
    let context = lash_core::testing::process_engine_run_context_for_validation(
        backend,
        registration.clone(),
        Arc::new(lash_core::ToolCatalog::default()),
        true,
    );
    let process_id = context.process_id().clone();
    let ctx = lash_core::testing::TestExecutionContextBuilder::new(
        lash_core::testing::TestExecutionPorts::of(backend),
    )
    .borrowed_effect_controller(scoped)
    .build()
    .into_runtime()
    .with_process_execution(
        process_id.clone(),
        &registration,
        lash_core::session::RuntimeExecutionProcessEventContext {
            execution_write_authority: lash_core::ProcessExecutionWriteAuthority::invocation(
                process_id.clone(),
                "host-law",
            )
            .bind_attempt(1),
            process_work: lash_core::testing::process_work_wiring_for_registry(
                backend.process_registry(),
            ),
            store: None,
            session_store_factory: None,
            queued_work: Arc::new(lash_core::NoSessionWork::new()),
            clock: backend.clock(),
        },
    );
    let identities = crate::LashlangHostIdentities::process_body(process_id.clone());
    let hash = lashlang::ContentHash::new("host-law");
    let execution_trace = lash_core::plugin::PluginExecutionTrace::new(context.trace_standing());
    let host = LashlangProcessHost {
        ctx,
        host_environment: Default::default(),
        artifact_store: lashlang::LashlangArtifacts::new(backend.module_artifacts()),
        workers: lash_vm_client::service::Service::default(),
        processes: context.processes(),
        process_id: process_id.clone(),
        run: crate::LashlangReplayRun::new(
            identities.namespace(),
            crate::LashlangRunOrdinals::start(),
        ),
        identities,
        producer: serde_json::json!({}),
        lashlang_execution_trace: LashlangProcessExecutionTrace::new(
            execution_trace,
            LashlangProcessTraceIdentity {
                session_id: None,
                process_id,
                source_identity: "host-law".into(),
                module_ref: lashlang::ModuleRef::new(&hash),
                process_ref: lashlang::ProcessRef::new(hash, 0),
                process_name: "main".into(),
                attempt: 1,
                engine_execution_id: None,
            },
        ),
        ordinals: ReplayOrdinals::restore(None),
        worker_recovery: Default::default(),
        cancellation: crate::ExecutionCancellation::new(),
        host_failure: Default::default(),
        effect_summary: Default::default(),
    };
    let failure = host
        .process_event(lashlang::ProcessEvent {
            kind: lashlang::ProcessEventKind::Yield,
            value: lashlang::Value::Null,
        })
        .await
        .expect_err("the registry refuses an unknown process");
    assert!(
        failure.message().contains("process"),
        "the law reaches the registry append: {failure}"
    );
    assert!(
        host.ctx.nested_replay_mismatch().is_none(),
        "the law reaches the host boundary"
    );
    assert!(
        host.is_cancelled(),
        "a host failure stops execution before the guest can catch it: {failure}"
    );
    let source = host
        .host_failure
        .lock_recover()
        .take()
        .expect("typed host cause");
    assert_eq!(
        serde_json::to_value(source).expect("returned cause"),
        serde_json::to_value(lash_core::PluginError::ProcessUnknown {
            process_id: host.process_id.clone()
        })
        .expect("original cause")
    );
}
