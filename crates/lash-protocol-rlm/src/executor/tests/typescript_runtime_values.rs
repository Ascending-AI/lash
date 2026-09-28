//! FIG-3079: the TypeScript dialect's journaled clock and RNG inside durable
//! processes.

use super::*;

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
    let artifact_store: lashlang::LashlangArtifacts =
        crate::testing::fresh_memory_artifact_store().await;
    let table = crate::testing::DoubleProcesses::new(0x3079_0001).await;
    let handler = table
        .open_handler(crate::testing::default_cell_scope())
        .await;
    let effect_host = table.backend().effect_host();
    let registry = table.registry();
    let process_env_store = table.env_store();
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
        table.backend().clone(),
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
    table.install_worker(
        lash_core::testing::test_code_protocol_factories(),
        runtime_host,
        session_policy.clone(),
    );
    let processes: Arc<dyn lash_core::ProcessService> = Arc::new(TypeScriptSignalProcessService {
        registry: registry.clone(),
        effect_host: Arc::clone(&effect_host),
        originator_override: None,
        env_store: Arc::clone(&process_env_store),
        engines: fixture_process_engines(artifact_store.clone(), surface.clone()),
    });
    let ctx = lash_core::testing::code_execution_context_with_process_dependencies(
        crate::testing::double_ports(table.double(), &handler),
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
    let response = execute_code_with_test_render(
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
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await;
    drop(ctx);
    handler.close().await.expect("close the cell's handler");
    assert!(response.error.is_none(), "{:?}", response.error);

    table.admit_pending().await;
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
    let terminal = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        table.await_terminal(&record.id),
    )
    .await
    {
        Ok(output) => output,
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
#[test]
pub(super) fn typescript_runtime_values_replay_from_the_journal_after_a_crash() {
    block_on(async {
        let session_id = "typescript-runtime-values";
        let turn_id = "turn-1";
        let double =
            crate::testing::kernel_double(0x3079, lash_restate_test::ServerConfig::default()).await;
        let backend = double.lash_backend();
        let samples = Arc::new(std::sync::Mutex::new(Vec::<(f64, f64)>::new()));
        let sample = |crash: bool| -> lash_restate_test::HandlerAttempt {
            let backend = backend.clone();
            let samples = Arc::clone(&samples);
            Arc::new(move |scoped| {
                let ctx = lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
                    crate::testing::attempt_ports(&backend, scoped),
                    Arc::new(ProcessControlToolProvider),
                    lash_core::ToolCatalog::default(),
                    lash_core::testing::exec_code_invocation(
                        session_id,
                        turn_id,
                        0,
                        0,
                        "runtime-values",
                        "replay:runtime-values",
                    ),
                );
                let samples = Arc::clone(&samples);
                Box::pin(async move {
                    // The alias the linker rewrites the lowerer's `builtin`
                    // receiver to once a module call is linked; before
                    // FIG-3079 this form never reached here.
                    let receiver = lashlang::Value::Resource(lashlang::ResourceHandle::new(
                        lashlang::LANGUAGE_RUNTIME_RESOURCE_TYPE,
                        lashlang::LANGUAGE_RUNTIME_MODULE_PATH,
                    ));
                    let number = async |operation: &str| {
                        let operation = lash_lashlang_runtime::typescript_runtime_operation(
                            &receiver,
                            operation,
                            &[],
                        )
                        .expect("the linked runtime receiver is recognised")
                        .expect("the runtime has the operation");
                        let value = lash_lashlang_runtime::journaled_typescript_runtime_value(
                            &ctx,
                            format!("typescript.runtime:{operation}"),
                            operation,
                        )
                        .await
                        .unwrap_or_else(|error| panic!("journaled `{operation}` journals: {error}"))
                        .unwrap_or_else(|error| {
                            panic!("journaled `{operation}` resolves: {error}")
                        });
                        match value {
                            lashlang::Value::Number(number) => number,
                            other => panic!("`{operation}` returns a number, got {other:?}"),
                        }
                    };
                    let now = number(lashlang::LANGUAGE_RUNTIME_NOW_OPERATION).await;
                    let random = number(lashlang::LANGUAGE_RUNTIME_RANDOM_OPERATION).await;
                    samples
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((now, random));
                    assert!(!crash, "the attempt's deployment dies after it sampled");
                })
            })
        };
        double
            .run_crashed_then_redriven(
                lash_core::AdmittedScope::turn(
                    lash_core::SessionId::from(session_id),
                    lash_core::TurnId::from(turn_id),
                ),
                sample(true),
                sample(false),
            )
            .await
            .expect("the sampling crashed and its redrive completed");
        let samples = samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let [(now, random), replayed] = samples.as_slice() else {
            panic!("one crashed attempt and one redrive sampled: {samples:?}");
        };
        assert!(*now > 0.0, "`now` samples a positive epoch, got {now}");
        assert!(
            (0.0..1.0).contains(random),
            "`random` stays in [0, 1), got {random}"
        );
        // The redrive replays the recorded ability outcomes rather than
        // sampling the clock and RNG again.
        assert_eq!(
            (*now, *random),
            *replayed,
            "replay must return the journaled samples"
        );
    });
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
