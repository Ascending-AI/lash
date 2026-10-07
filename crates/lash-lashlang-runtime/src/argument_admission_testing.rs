//! Argument-admission laws over a host's module artifact store.
#![expect(clippy::expect_used, reason = "conformance fixture assertions")]
use super::*;
use lashlang::testing::ast_builders as b;

pub async fn process_event_host_failure_stops_execution(
    backend: &lash_core::Backend,
    scoped: lash_core::ActorContext,
) {
    Box::pin(crate::process::process_event_host_failure_stops_execution(
        backend, scoped,
    ))
    .await;
}

/// A real worker-backed process must return the run guard's typed failure.
pub async fn process_shutdown_preserves_typed_failures(
    backend: &lash_core::Backend,
    scoped: lash_core::ActorContext,
) {
    use lash_core::ProcessEngine as _;

    let environment = LashlangHostEnvironment::default();
    let compiled = compile_fixture(
        "process guarded() -> null { finish null }",
        process_module("guarded", Vec::new(), lashlang::TypeExpr::Null, b::null()),
        &environment,
    )
    .await;
    let store = LashlangArtifacts::new(backend.module_artifacts());
    store
        .publish_module_artifact(&host_claim(), &compiled.artifact)
        .await
        .expect("publish the process");
    let input = LashlangProcessInput {
        module_ref: compiled.module_ref,
        process_ref: compiled
            .artifact
            .process_ref("guarded")
            .expect("export")
            .clone(),
        host_requirements_ref: compiled.host_requirements_ref,
        process_name: "guarded".into(),
        args: serde_json::Map::new(),
    };
    for code in [
        lash_core::RuntimeErrorCode::LashlangCellReplayDivergence,
        lash_core::RuntimeErrorCode::RuntimeStoreCorrupt,
        lash_core::RuntimeErrorCode::RuntimeStore,
    ] {
        let engine = LashlangProcessEngine::new(
            store.clone(),
            LashlangSurface::default(),
            backend.worker_recovery(),
        );
        let mut registration = lash_core::ProcessRegistration::new(
            input.to_process_input().expect("input"),
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        )
        .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
            input.process_identity(),
        ));
        registration.engine_config = engine
            .creation_config(&lash_core::ProcessExecutionEnvSpec::new(
                lash_core::AdmittedPluginConfig::default(),
                lash_core::SessionPolicy::new(
                    lash_core::TurnBudget::Unbounded,
                    lash_core::MaxToolCalls::new(1024),
                ),
            ))
            .expect("record settings");
        let process_id = lash_core::mint_process_id();
        let built = lash_core::testing::TestExecutionContextBuilder::new(
            lash_core::testing::TestExecutionPorts::of(backend),
        )
        .process_engines(
            lash_core::ProcessEngineRegistry::new()
                .with_registration(lashlang_process_engine_registration(engine.clone()))
                .with_artifact_ports(lash_core::ArtifactReferrerPorts::of_backend(backend)),
        )
        .borrowed_effect_controller(scoped.clone())
        .build();
        let plugins = Arc::clone(&built.dispatch.plugins);
        let catalog = Arc::clone(&built.dispatch.tool_catalog);
        let runtime = built.into_runtime();
        let source = lash_core::PluginError::RuntimeEffectController(
            lash_core::RuntimeEffectControllerError::new(code, "typed shutdown witness"),
        );
        let expected = serde_json::to_value(&source).expect("encode cause");
        let expected_park = source.park_reason();
        let expected_retry = source.is_retryable();
        let expected_terminal = source.is_terminal();
        let shut_down = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = Arc::clone(&shut_down);
        let context = lash_core::ProcessEngineRunContext::new(
            registration,
            process_id.clone(),
            lash_core::ProcessExecutionContext::default().with_execution_write_authority(
                lash_core::ProcessExecutionWriteAuthority::invocation(process_id, "guard-law")
                    .bind_attempt(1),
            ),
            lash_core::testing::process_work_wiring_for_registry(backend.process_registry()),
            plugins,
            catalog,
            None,
            None,
            Arc::new(lash_core::NoSessionWork::new()),
            backend.clock(),
            true,
            lash_core::CancellationToken::new(),
            None,
            scoped.clone(),
            None,
            Box::new(move |_| {
                Ok(lash_core::runtime::ProcessEngineRuntimeContext::new(
                    runtime,
                    lash_core::runtime::ProcessEngineRunGuard::new(move |parent_ended| {
                        Box::pin(async move {
                            assert!(!parent_ended);
                            observed.store(true, std::sync::atomic::Ordering::SeqCst);
                            Err(source)
                        })
                    }),
                ))
            }),
        );
        let error = Box::pin(crate::process::run_lashlang_process(
            engine,
            &scoped,
            context,
            serde_json::to_value(&input).expect("payload"),
        ))
        .await
        .expect_err("guard aborts the run")
        .into_plugin_error();
        assert!(
            shut_down.load(std::sync::atomic::Ordering::SeqCst),
            "the real engine reaches shutdown: {error:?}"
        );
        assert_eq!(
            serde_json::to_value(&error).expect("encode returned cause"),
            expected
        );
        assert_eq!(error.park_reason(), expected_park);
        assert_eq!(error.is_retryable(), expected_retry);
        assert_eq!(error.is_terminal(), expected_terminal);
    }
}
fn process_module(
    name: &str,
    params: Vec<lashlang::ProcessParam>,
    output: lashlang::TypeExpr,
    body: lashlang::Expr,
) -> lashlang::Program {
    b::module(
        vec![b::process_returning(name, params, output, b::finish(body))],
        Vec::new(),
    )
}
fn host_claim() -> lash_core::ReferrerClaim {
    lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
        lash_core::HostArtifactPin::mint(),
    ))
    .expect("unguarded host pin")
}
async fn compile_fixture(
    source: &str,
    program: lashlang::Program,
    environment: &LashlangHostEnvironment,
) -> lash_vm_client::service::CompiledModule {
    match lash_vm_client::service::Service::default()
        .request_accounted(lash_vm_client::service::Request::CompileAst {
            source: source.to_owned(),
            program,
            environment: environment.clone(),
        })
        .await
        .expect("compile fixture in worker")
    {
        lash_vm_client::service::Response::Module(module) => *module,
        response => panic!("worker refused fixture: {response:?}"),
    }
}
pub async fn nested_process_arguments_reject_forged_aliases_and_try_later_union_arms(
    store: Arc<dyn lash_core::ModuleArtifactStore>,
) {
    let store = LashlangArtifacts::new(store);
    let environment = LashlangHostEnvironment::default();
    let handler = compile_fixture(
        "nested handler",
        process_module(
            "handler",
            vec![
                b::param("event", lashlang::TypeExpr::Str),
                b::param("other", lashlang::TypeExpr::Str),
            ],
            lashlang::TypeExpr::Bool,
            b::bool_lit(true),
        ),
        &environment,
    )
    .await;
    let process_type = b::process_type(
        vec![
            b::param("event", lashlang::TypeExpr::Str),
            b::param("other", lashlang::TypeExpr::Str),
        ],
        lashlang::TypeExpr::Bool,
    );
    let receiver = compile_fixture(
        "nested union receiver",
        process_module(
            "install",
            vec![b::param(
                "envelope",
                lashlang::TypeExpr::Object(vec![b::type_field(
                    "handlers",
                    lashlang::TypeExpr::List(Box::new(lashlang::TypeExpr::union(vec![
                        process_type,
                        lashlang::TypeExpr::Str,
                    ]))),
                    false,
                )]),
            )],
            lashlang::TypeExpr::Bool,
            b::bool_lit(true),
        ),
        &environment,
    )
    .await;
    for artifact in [&handler.artifact, &receiver.artifact] {
        store
            .publish_module_artifact(&host_claim(), artifact)
            .await
            .expect("publish artifact");
    }
    let valid = handler
        .artifact
        .definition_identity("handler")
        .expect("handler identity")
        .to_process_value();
    let start = |value| {
        let mut args = lashlang::Record::new();
        args.insert(
            "envelope".into(),
            lashlang::from_json(
                serde_json::json!({"handlers":[value,"later nonprocess union arm"]}),
            ),
        );
        lashlang::ProcessStart {
            module_ref: receiver.module_ref.clone(),
            process_ref: receiver
                .artifact
                .process_ref("install")
                .expect("receiver export")
                .clone(),
            host_requirements_ref: receiver.host_requirements_ref.clone(),
            start_site: lashlang::LashlangExecutionCallSite {
                site: lashlang::LashlangExecutionSite {
                    node_id: "nested-union".into(),
                    node_kind: lash_sansio::ExecutionNodeKind::Call,
                    label: "nested start".into(),
                    branch: None,
                    workflow_site: lashlang::WorkflowExecutionSite::new(
                        "process:install",
                        [],
                        lash_sansio::ExecutionNodeKind::Call,
                        "nested start",
                    ),
                },
                occurrence: 1,
            },
            process_name: "install".into(),
            args,
        }
    };
    let artifacts: LashlangArtifacts = store;
    prepare_lashlang_process_start(
        &lash_vm_client::service::Service::default(),
        artifacts.clone(),
        None,
        start(valid.clone()),
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached.into(),
    )
    .await
    .expect("later union arm and honest nested identity pass");
    for field in ["process_name", "module_ref", "host_requirements_ref"] {
        let mut forged = valid.clone();
        forged[field] = serde_json::json!("forged-alias");
        let error = prepare_lashlang_process_start(
            &lash_vm_client::service::Service::default(),
            artifacts.clone(),
            None,
            start(forged),
            lash_core::ProcessOriginator::host(),
            lash_core::Lifetime::Detached.into(),
        )
        .await
        .expect_err("forged nested alias refuses before a start request exists");
        assert!(
            matches!(error, LashlangRuntimeError::InvalidProcessArgument { ref path, .. } if path == "envelope.handlers[0]"),
            "{error:?}"
        );
    }
}
