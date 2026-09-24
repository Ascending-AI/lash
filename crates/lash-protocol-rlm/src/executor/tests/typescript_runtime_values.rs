//! FIG-3079: the TypeScript dialect's journaled clock and RNG inside durable
//! processes.

use super::*;
use lash_core::testing::store_fixtures::durable_admission;

/// FIG-3079: `new Date()`, `Date.now()` and `Math.random()` inside a durable
/// process body.
///
/// The TypeScript lowerer mints these as a module call on the reserved
/// `typescript.Runtime` receiver, and linking rewrites that receiver's alias
/// from the lowerer's `builtin` to the module-path key `__typescript_runtime`.
/// While the process host gated its journaled short-circuit on the `builtin`
/// alias, a lifted body fell through to catalog tool resolution and failed with
/// "module operation `now` resolved to unavailable host operation
/// `typescript.runtime.now`" before the process could complete.
#[tokio::test]
pub(super) async fn typescript_process_body_resolves_journaled_clock_and_randomness() {
    let artifact_store: Arc<dyn lashlang::LashlangArtifactStore> =
        crate::testing::fresh_memory_artifact_store().await;
    let backend = memory_backend().await;
    let registry = backend.process_registry();
    let process_env_store = backend.process_env_store();
    let effect_host = backend.effect_host();
    let surface = LashlangSurface::new(
        lashlang::LashlangAbilities::default(),
        lashlang::LashlangLanguageFeatures::default(),
        lashlang::LashlangHostCatalog::new(),
    );
    let session_policy = lash_core::SessionPolicy {
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("TypeScript runtime-value process test model"),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let runtime_host = lash_core::facade_support::RuntimeHostConfig::new(
        Arc::clone(&backend),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_process_engine_registration(
        lash_lashlang_runtime::lashlang_process_engine_registration(
            lash_lashlang_runtime::LashlangProcessEngine::new(
                artifact_store.clone(),
                process_engine_surface(surface.clone()),
            ),
        ),
    );
    let registry_dyn = Arc::clone(&registry);
    let watched = lash_core::facade_support::watch_process_registry(registry_dyn);
    let worker = lash_core_worker::DurableProcessWorker::new(
        lash_core_worker::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::new(
                lash_core::testing::test_code_protocol_factories(),
            )),
            runtime_host,
            lash_core_worker::WorkerProcessWork::SelfNative(watched),
            Arc::new(lash_core::NoQueuedWork::new()),
            lash_core::testing::runtime_lease_owner(),
        )
        .with_session_policy(session_policy.clone()),
    )
    .expect("valid test native substrate config");
    let processes: Arc<dyn lash_core::ProcessService> = Arc::new(TypeScriptSignalProcessService {
        registry: registry.clone(),
        effect_host: Arc::clone(&effect_host),
        originator_override: None,
        env_store: Arc::clone(&process_env_store),
        engines: fixture_process_engines(artifact_store.clone(), surface.clone()),
    });
    let ctx = lash_core::testing::code_execution_context_with_process_dependencies(
        lash_core::testing::TestExecutionPorts::over_host(effect_host, process_env_store),
        Arc::new(ProcessControlToolProvider),
        process_control_tool_catalog(),
        None,
        processes,
        lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            session_policy,
        ),
    );
    let mut state = RlmExecutionState::for_engine("typescript");
    let response = execute_code_with_channel_and_bounds(
        &mut state,
        ctx.clone(),
        ExecRequest {
            language: "typescript".to_string(),
            code: r#"
                    const worker = async () => {
                      const stamp = new Date().toISOString();
                      const ms = Date.now();
                      const roll = Math.random();
                      return { stamp: stamp, ms: ms, roll: roll };
                    };
                    await processes.start({ definition: worker });
                    finish("started");
                "#
            .to_string(),
        },
        artifact_store.clone(),
        surface.clone(),
        None,
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);

    let _ = worker
        .drive_pending_processes()
        .await
        .expect("drive the runtime-value TypeScript process");
    let records = registry
        .list_observed_by(
            &SessionId::from("test-session"),
            &lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .expect("list the started TypeScript process");
    let [record] = records.as_slice() else {
        panic!("expected exactly one started TypeScript process, got {records:?}");
    };
    let registry_dyn = Arc::clone(&registry);
    let terminal = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        lash_core::NativeProcessWork::for_registry(registry_dyn).await_terminal(&record.id),
    )
    .await
    {
        Ok(output) => output.expect("await the runtime-value TypeScript process"),
        Err(_) => panic!(
            "runtime-value TypeScript process reaches terminal state: {:?}",
            registry.get_process(&record.id).await
        ),
    };
    let output = terminal.into_tool_output();
    assert!(
        matches!(output.outcome, lash_core::ToolCallOutcome::Success(_)),
        "the process completes instead of failing: {:?}",
        output.outcome
    );
    let value = output.value_for_projection();
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("process returns a record, got {value}"));
    let stamp = object["stamp"].as_str().expect("ISO stamp is a string");
    assert!(
        stamp.ends_with('Z') && stamp.len() == 24 && stamp.as_bytes()[10] == b'T',
        "`new Date().toISOString()` returns an ISO-8601 stamp, got {stamp}"
    );
    let ms = object["ms"]
        .as_f64()
        .expect("`Date.now()` returns a number");
    assert!(ms > 0.0, "`Date.now()` returns a positive epoch, got {ms}");
    let roll = object["roll"]
        .as_f64()
        .expect("`Math.random()` returns a number");
    assert!(
        (0.0..1.0).contains(&roll),
        "`Math.random()` stays in [0, 1), got {roll}"
    );
}

/// FIG-3079: the journaled clock and RNG never re-sample on replay, and the
/// linked receiver alias (`__typescript_runtime`) reaches the journal the same
/// way the lowerer's `builtin` alias does.
#[tokio::test]
pub(super) async fn typescript_runtime_values_replay_from_the_journal_after_reopen() {
    let dir = tempfile::tempdir().expect("temporary effect journal");
    let path = dir.path().join("typescript-runtime-values.sqlite");
    let session_id = "typescript-runtime-values";
    let turn_id = "turn-1";
    let scope = lash_core::ExecutionScope::turn(session_id, turn_id);
    // The alias the linker rewrites the lowerer's `builtin` receiver to once a
    // module call is linked; before FIG-3079 this form never reached here.
    let receiver = lashlang::Value::Resource(lashlang::ResourceHandle::new(
        lashlang::LANGUAGE_RUNTIME_RESOURCE_TYPE,
        lashlang::LANGUAGE_RUNTIME_MODULE_PATH,
    ));

    async fn sample(
        path: &std::path::Path,
        scope: &lash_core::ExecutionScope,
        session_id: &str,
        turn_id: &str,
        receiver: &lashlang::Value,
    ) -> (f64, f64) {
        let controller =
            lash_sqlite_store::SqliteRuntimeEffectController::open(path, scope.clone())
                .await
                .expect("open SQLite effect controller");
        let ctx = lash_core::testing::code_execution_context_with_tool_provider_catalog_scoped_effect_controller_and_invocation(crate::testing::memory_backend_ports().await, Arc::new(ProcessControlToolProvider), lash_core::ToolCatalog::default(), lash_core::ScopedEffectController::shared(Arc::new(controller), durable_admission(scope))
                .expect("admit SQLite controller scope"), lash_core::testing::exec_code_invocation(
                session_id,
                turn_id,
                0,
                0,
                "runtime-values",
                "replay:runtime-values",
            ));
        let number = async |operation: &str| {
            let operation =
                lash_lashlang_runtime::typescript_runtime_operation(receiver, operation, &[])
                    .expect("the linked runtime receiver is recognised")
                    .expect("the runtime has the operation");
            let value = lash_lashlang_runtime::journaled_typescript_runtime_value(
                &ctx,
                format!("typescript.runtime:{operation}"),
                operation,
            )
            .await
            .unwrap_or_else(|error| panic!("journaled `{operation}` journals: {error}"))
            .unwrap_or_else(|error| panic!("journaled `{operation}` resolves: {error}"));
            match value {
                lashlang::Value::Number(number) => number,
                other => panic!("`{operation}` returns a number, got {other:?}"),
            }
        };
        let now = number(lashlang::LANGUAGE_RUNTIME_NOW_OPERATION).await;
        let random = number(lashlang::LANGUAGE_RUNTIME_RANDOM_OPERATION).await;
        (now, random)
    }

    let (now, random) = sample(&path, &scope, session_id, turn_id, &receiver).await;
    assert!(now > 0.0, "`now` samples a positive epoch, got {now}");
    assert!(
        (0.0..1.0).contains(&random),
        "`random` stays in [0, 1), got {random}"
    );

    // A cold reopen replays the recorded ability outcomes rather than sampling
    // the clock and RNG again.
    let replayed = sample(&path, &scope, session_id, turn_id, &receiver).await;
    assert_eq!(
        (now, random),
        replayed,
        "replay must return the journaled samples"
    );
}

/// FIG-3079: a journaled float must decode to the double that was written.
///
/// serde_json's default float parser is a fast approximation that lands one
/// ULP off for roughly a tenth of all doubles; the workspace therefore pins
/// the `float_roundtrip` feature. Without it every journaled `Math.random()`
/// sample has about a one-in-ten chance of replaying as a different number,
/// and so does every other float that crosses an effect journal.
#[test]
pub(super) fn journaled_floats_decode_to_the_double_that_was_written() {
    let mut mismatches = Vec::new();
    for index in 0..50_000u64 {
        // The same construction the journaled `random` executor uses: the low
        // 53 bits of a draw, mapped onto the unit interval.
        let bits = ((u128::from(index) * 0x9E37_79B9_7F4A_7C15_u128) & ((1_u128 << 53) - 1)) as u64;
        let written = bits as f64 / ((1_u64 << 53) as f64);
        let encoded = serde_json::to_string(&written).expect("encode a journaled float");
        let decoded: f64 = serde_json::from_str(&encoded).expect("decode a journaled float");
        if decoded != written {
            mismatches.push(encoded);
        }
    }
    assert!(
        mismatches.is_empty(),
        "journaled floats must replay exactly; {} of 50000 drifted, e.g. {:?}",
        mismatches.len(),
        &mismatches[..mismatches.len().min(3)]
    );
}
