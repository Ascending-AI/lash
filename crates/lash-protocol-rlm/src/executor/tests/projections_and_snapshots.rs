use super::*;

#[test]
pub(super) fn projected_history_is_available_without_clobbering_executor_globals() {
    block_on(async {
        let mut state = RlmExecutionState::new();
        let mut set_default = serde_json::Map::new();
        set_default.insert("diary".to_string(), serde_json::json!(["kept"]));
        state
            .patch_globals(
                &lash_rlm_types::RlmGlobalsPatchPluginBody { set_default },
                &BTreeSet::new(),
            )
            .expect("patch diary");

        let projected = projected_history(vec![FlowValue::String("hello".into())]);
        let compiled =
            lashlang::compile("finish { history_len: len(history), diary_len: len(diary) }")
                .expect("compile");
        let outcome = execute_with_projected(&compiled, &mut state.rlm, &projected)
            .await
            .expect("execute");
        let ExecutionOutcome::Finished(FlowValue::Record(record)) = outcome else {
            panic!("expected finishted record");
        };
        assert_eq!(record["history_len"], FlowValue::Number(1.0));
        assert_eq!(record["diary_len"], FlowValue::Number(1.0));
        assert!(state.rlm.snapshot().globals().get("history").is_none());
    });
}

#[test]
pub(super) fn projected_history_defaults_to_empty_list_when_missing() {
    block_on(async {
        let mut state = RlmExecutionState::new();

        let projected = projected_history(Vec::new());
        let compiled = lashlang::compile("finish { history_len: len(history) }").expect("compile");
        let outcome = execute_with_projected(&compiled, &mut state.rlm, &projected)
            .await
            .expect("execute");
        let ExecutionOutcome::Finished(FlowValue::Record(record)) = outcome else {
            panic!("expected finishted record");
        };
        assert_eq!(record["history_len"], FlowValue::Number(0.0));
    });
}

#[test]
pub(super) fn set_default_initializes_once_and_does_not_mutate_projected_globals() {
    let mut state = RlmExecutionState::new();
    let projected = BTreeSet::from_iter(["current_query".to_string()]);

    state
        .patch_globals(
            &lash_rlm_types::RlmGlobalsPatchPluginBody {
                set_default: serde_json::Map::from_iter([(
                    "diary".to_string(),
                    serde_json::json!(["initial"]),
                )]),
            },
            &projected,
        )
        .expect("apply defaults");
    assert_eq!(
        state.rlm.snapshot().globals().get("diary"),
        Some(&FlowValue::List(
            vec![FlowValue::String("initial".into())].into()
        ))
    );
    assert!(
        state
            .rlm
            .snapshot()
            .globals()
            .get("current_query")
            .is_none()
    );

    state
        .patch_globals(
            &lash_rlm_types::RlmGlobalsPatchPluginBody {
                set_default: serde_json::Map::from_iter([(
                    "diary".to_string(),
                    serde_json::json!(["clobber"]),
                )]),
            },
            &projected,
        )
        .expect("reapply defaults");
    assert_eq!(
        state.rlm.snapshot().globals().get("diary"),
        Some(&FlowValue::List(
            vec![FlowValue::String("initial".into())].into()
        ))
    );
}

#[test]
pub(super) fn heap_backed_default_patch_survives_next_cell_and_cold_restore() {
    block_on(async {
        let projected = ProjectedBindings::new();
        let mut state = RlmExecutionState::new();
        let setup = lashlang::compile("seed = [{ nested: [1] }]").expect("compile setup");
        execute_with_projected(&setup, &mut state.rlm, &projected)
            .await
            .expect("execute setup");
        state
            .patch_globals(
                &lash_rlm_types::RlmGlobalsPatchPluginBody {
                    set_default: serde_json::Map::from_iter([(
                        "diary".to_string(),
                        serde_json::json!(["kept"]),
                    )]),
                },
                &BTreeSet::new(),
            )
            .expect("patch heap-backed state");

        let finish = lashlang::compile("finish diary").expect("compile finish");
        assert_eq!(
            execute_with_projected(&finish, &mut state.rlm, &projected)
                .await
                .expect("next cell sees patch"),
            ExecutionOutcome::Finished(FlowValue::List(
                vec![FlowValue::String("kept".into())].into()
            ))
        );

        let bytes = state
            .rlm
            .snapshot()
            .to_canonical_bytes()
            .expect("encode patched state");
        let snapshot =
            lashlang::Snapshot::from_canonical_bytes(&bytes).expect("decode patched state");
        let mut restored = lashlang::State::from_snapshot(snapshot);
        assert_eq!(
            execute_with_projected(&finish, &mut restored, &projected)
                .await
                .expect("cold-restored cell sees patch"),
            ExecutionOutcome::Finished(FlowValue::List(
                vec![FlowValue::String("kept".into())].into()
            ))
        );
    });
}

#[test]
pub(super) fn rejected_global_patch_leaves_byte_identical_state_and_no_dirty_marks() {
    block_on(async {
        let projected = ProjectedBindings::new();
        let mut state = RlmExecutionState::new();
        let setup = lashlang::compile("seed = [{ nested: [1] }]").expect("compile setup");
        execute_with_projected(&setup, &mut state.rlm, &projected)
            .await
            .expect("execute setup");
        let before = state
            .rlm
            .snapshot()
            .to_canonical_bytes()
            .expect("encode pre-patch state");
        let dirty_before = state.execution_state_dirty();

        // A deterministically ordered patch whose first key is acceptable
        // and whose second is the reserved binding. Applying keys one at a
        // time committed `a_good` and then failed, leaving a mutation no
        // dirty mark accounted for.
        let error = state
            .patch_globals(
                &lash_rlm_types::RlmGlobalsPatchPluginBody {
                    set_default: serde_json::Map::from_iter([
                        ("a_good".to_string(), serde_json::json!(["kept"])),
                        ("history".to_string(), serde_json::json!(["nope"])),
                    ]),
                },
                &BTreeSet::new(),
            )
            .expect_err("a reserved name must reject the whole patch");
        assert!(error.to_string().contains("history"));

        assert!(
            state.rlm.globals().get("a_good").is_none(),
            "no key from a rejected patch may be committed"
        );
        assert_eq!(
            state
                .rlm
                .snapshot()
                .to_canonical_bytes()
                .expect("encode post-patch state"),
            before,
            "a rejected patch must leave the state byte-identical"
        );
        assert_eq!(
            state.execution_state_dirty(),
            dirty_before,
            "a rejected patch must not mark the execution state dirty"
        );
    });
}

#[test]
pub(super) fn rejected_protected_name_patch_leaves_byte_identical_state() {
    block_on(async {
        let projected = ProjectedBindings::new();
        let mut state = RlmExecutionState::new();
        let setup = lashlang::compile("seed = [1]").expect("compile setup");
        execute_with_projected(&setup, &mut state.rlm, &projected)
            .await
            .expect("execute setup");
        let before = state
            .rlm
            .snapshot()
            .to_canonical_bytes()
            .expect("encode pre-patch state");

        let protected = BTreeSet::from(["docs".to_string()]);
        state
            .patch_globals(
                &lash_rlm_types::RlmGlobalsPatchPluginBody {
                    set_default: serde_json::Map::from_iter([
                        ("a_good".to_string(), serde_json::json!(1)),
                        ("docs".to_string(), serde_json::json!(2)),
                    ]),
                },
                &protected,
            )
            .expect_err("a protected name must reject the whole patch");

        assert_eq!(
            state
                .rlm
                .snapshot()
                .to_canonical_bytes()
                .expect("encode post-patch state"),
            before
        );
    });
}

#[test]
pub(super) fn heap_backed_projection_rehydrate_and_prune_survive_execution_and_restore() {
    block_on(async {
        let projected = ProjectedBindings::new();
        let setup = lashlang::compile("history = [{ role: \"user\" }]\nkept = [{ nested: [2] }]")
            .expect("compile setup");
        let mut state = lashlang::State::new();
        execute_with_projected(&setup, &mut state, &projected)
            .await
            .expect("execute setup");

        let registry = Arc::new(ProjectionRegistry::new());
        let descriptor = Arc::new(SnapshotProjectedToolText::default());
        let reference = registry.register_memory(descriptor.clone());
        state
            .insert_global(
                "doc",
                FlowValue::Projected(ProjectedValue::custom_with_projection_ref(
                    "doc",
                    descriptor,
                    serde_json::to_value(&reference).expect("projection ref"),
                )),
            )
            .expect("insert projected value");
        let unavailable = state
            .snapshot()
            .to_canonical_bytes()
            .expect("encode projected state");
        state = lashlang::State::from_snapshot(
            lashlang::Snapshot::from_canonical_bytes(&unavailable)
                .expect("restore projected state as unavailable"),
        );
        rehydrate_projected_globals(
            &mut state,
            Arc::clone(&registry) as Arc<dyn ProjectionResolver>,
        )
        .await
        .expect("rehydrate heap-backed projected value");
        crate::projection::prune_reserved_projected_bindings(&mut state);

        let finish =
            lashlang::compile("finish { doc: doc, kept: kept }").expect("compile post-patch read");
        let expected =
            ExecutionOutcome::Finished(FlowValue::Record(Arc::new(FlowRecord::from_iter([
                (
                    "doc".to_string(),
                    FlowValue::String("materialized tool text".into()),
                ),
                (
                    "kept".to_string(),
                    FlowValue::List(
                        vec![FlowValue::Record(Arc::new(FlowRecord::from_iter([(
                            "nested".to_string(),
                            FlowValue::List(vec![FlowValue::Number(2.0)].into()),
                        )])))]
                        .into(),
                    ),
                ),
            ]))));
        assert_eq!(
            execute_with_projected(&finish, &mut state, &projected)
                .await
                .expect("next cell sees rehydrate and prune"),
            expected
        );
        assert!(state.globals().get("history").is_none());

        let bytes = state
            .snapshot()
            .to_canonical_bytes()
            .expect("encode rehydrated and pruned state");
        let snapshot = lashlang::Snapshot::from_canonical_bytes(&bytes)
            .expect("decode rehydrated and pruned state");
        let mut restored = lashlang::State::from_snapshot(snapshot);
        rehydrate_projected_globals(
            &mut restored,
            Arc::clone(&registry) as Arc<dyn ProjectionResolver>,
        )
        .await
        .expect("rehydrate after cold restore");
        assert_eq!(
            execute_with_projected(&finish, &mut restored, &projected)
                .await
                .expect("cold-restored cell sees rehydrate and prune"),
            expected
        );
        assert!(restored.globals().get("history").is_none());
    });
}

pub(super) fn restored_projection_degradation_fixture()
-> (RlmExecutionState, Arc<ProjectionRegistry>) {
    let registry = Arc::new(ProjectionRegistry::new());
    let descriptor = Arc::new(SnapshotProjectedToolText::default());
    let healthy_reference = registry.register_memory(descriptor.clone());
    let dead_reference =
        ProjectionRef::new("memory", serde_json::json!("missing")).with_descriptor_type("string");
    let mut source = RlmExecutionState::new();
    for (name, reference) in [("healthy", healthy_reference), ("dead", dead_reference)] {
        source
            .rlm
            .insert_global(
                name.to_string(),
                FlowValue::Projected(ProjectedValue::custom_with_projection_ref(
                    name,
                    descriptor.clone(),
                    serde_json::to_value(reference).expect("projection ref"),
                )),
            )
            .expect("insert projected global");
    }
    source
        .rlm
        .insert_global("ordinary".to_string(), FlowValue::String("kept".into()))
        .expect("insert ordinary global");
    let snapshot = hydrate_snapshot(
        source
            .snapshot_execution_state()
            .expect("snapshot projected globals"),
    );
    let mut restored = RlmExecutionState::new();
    restored
        .restore_execution_state(&snapshot)
        .expect("restore projected globals as unavailable references");
    (restored, registry)
}

#[test]
pub(super) fn one_dead_projection_degrades_only_its_binding_and_errors_by_name_at_touch() {
    block_on(async {
        let (mut state, registry) = restored_projection_degradation_fixture();
        let response = execute_code_unbounded_for_tests(
            &mut state,
            lash_core::testing::code_execution_context(),
            ExecRequest {
                language: "lashlang".to_string(),
                code: "print healthy\nprint dead\nfinish ordinary".to_string(),
            },
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            registry as Arc<dyn ProjectionResolver>,
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;

        assert_eq!(response.error, None);
        assert_eq!(response.terminal_finish, Some(serde_json::json!("kept")));
        assert_eq!(response.degraded_bindings.len(), 1);
        assert_eq!(response.degraded_bindings[0].name, "dead");
        assert!(
            response.degraded_bindings[0]
                .reason
                .contains("projection ref unavailable")
        );
        let touched = response
            .observations
            .iter()
            .map(|observation| observation.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(touched.contains("rendered tool text"), "{touched}");
        assert!(
            touched.contains("projected host descriptor `dead`"),
            "{touched}"
        );
        assert!(
            touched.contains("unavailable after snapshot restore"),
            "{touched}"
        );
    });
}

#[test]
pub(super) fn strict_host_policy_can_abort_on_the_degraded_binding_list() {
    fn strict_host_policy(bindings: &[lash_core::DegradedBinding]) -> Result<(), String> {
        if bindings.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "strict host rejected degraded bindings: {}",
                bindings
                    .iter()
                    .map(|binding| binding.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
    }

    block_on(async {
        let (mut state, registry) = restored_projection_degradation_fixture();
        let response = execute_code_unbounded_for_tests(
            &mut state,
            lash_core::testing::code_execution_context(),
            ExecRequest {
                language: "lashlang".to_string(),
                code: "finish healthy".to_string(),
            },
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::default(),
            None,
            RlmProjectedBindings::default(),
            registry as Arc<dyn ProjectionResolver>,
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;

        let error = strict_host_policy(&response.degraded_bindings)
            .expect_err("strict host policy must reject degraded setup");
        assert_eq!(error, "strict host rejected degraded bindings: dead");
    });
}

#[test]
pub(super) fn set_default_rejects_projected_host_bindings() {
    let mut state = RlmExecutionState::new();
    let projected = BTreeSet::from_iter(["current_query".to_string()]);

    let err = state
        .patch_globals(
            &lash_rlm_types::RlmGlobalsPatchPluginBody {
                set_default: serde_json::Map::from_iter([(
                    "current_query".to_string(),
                    serde_json::json!("bad"),
                )]),
            },
            &projected,
        )
        .expect_err("projected default should fail");
    assert!(err.to_string().contains("read-only projected host binding"));

    let err = state
        .patch_globals(
            &lash_rlm_types::RlmGlobalsPatchPluginBody {
                set_default: serde_json::Map::from_iter([(
                    "history".to_string(),
                    serde_json::json!([]),
                )]),
            },
            &BTreeSet::new(),
        )
        .expect_err("history default should fail");
    assert!(err.to_string().contains("read-only projected host binding"));
}

#[test]
pub(super) fn projected_scalar_bindings_are_read_only_and_not_snapshotted() {
    block_on(async {
        let mut state = RlmExecutionState::new();
        let mut projected = ProjectedBindings::new();
        projected.insert(
            "current_query",
            ProjectedValue::scalar("current_query", FlowValue::String("host".into())),
        );

        let compiled =
            lashlang::compile("finish { chars: len(current_query), value: current_query }")
                .expect("compile read");
        let outcome = execute_with_projected(&compiled, &mut state.rlm, &projected)
            .await
            .expect("execute read");
        let ExecutionOutcome::Finished(FlowValue::Record(record)) = outcome else {
            panic!("expected finishted record");
        };
        assert_eq!(record["chars"], FlowValue::Number(4.0));
        assert_eq!(record["value"], FlowValue::String("host".into()));
        assert!(
            state
                .rlm
                .snapshot()
                .globals()
                .get("current_query")
                .is_none()
        );

        let compiled = lashlang::compile("current_query = \"local\"").expect("compile write");
        let env = ExecutionEnvironment::new(&NoopHost)
            .traced()
            .with_projected_bindings(projected.clone());
        let error = lashlang::execute(&compiled, &mut state.rlm, &env)
            .await
            .expect_err("projected write should fail");
        let failure = env
            .take_runtime_failure()
            .unwrap_or(lashlang::RuntimeFailure { error, span: None });
        assert!(
            failure
                .error
                .to_string()
                .contains("read-only projected binding")
        );
    });
}

#[test]
pub(super) fn executor_snapshot_does_not_materialize_projected_tool_result_globals() {
    let projected = Arc::new(SnapshotProjectedToolText::default());
    let mut state = RlmExecutionState::new();
    state
        .rlm
        .insert_global(
            "m".to_string(),
            FlowValue::Projected(ProjectedValue::custom(
                "search.matches[0].text",
                projected.clone(),
            )),
        )
        .expect("insert projected global");

    let snapshot = hydrate_snapshot(state.snapshot_execution_state().expect("executor snapshot"));
    assert_eq!(projected.render_count.load(Ordering::SeqCst), 0);
    assert_eq!(projected.materialize_count.load(Ordering::SeqCst), 0);
    let mut encoded = snapshot.root.clone();
    for body in snapshot.components.values() {
        encoded.extend_from_slice(body);
    }
    let encoded_text = String::from_utf8_lossy(&encoded);
    assert!(!encoded_text.contains("rendered tool text"));
    assert!(!encoded_text.contains("materialized tool text"));

    let mut restored_execution = RlmExecutionState::new();
    restored_execution
        .restore_execution_state(&snapshot)
        .expect("restore runtime");
    let restored = restored_execution.rlm;
    assert!(matches!(
        restored.snapshot().globals().get("m"),
        Some(FlowValue::Projected(_))
    ));
}

#[test]
pub(super) fn measured_commit_budget_carries_only_changed_leaf_bodies() {
    block_on(async {
        let mut source = String::new();
        for index in 0..12 {
            let payload = format!("large-{index}-{}", "x".repeat(6 * 1024));
            source.push_str(&format!("large_{index} = [\"{payload}\"]\n"));
        }
        for index in 0..40 {
            source.push_str(&format!("small_{index} = {index}\n"));
        }
        let mut state = execute_test_code(RlmExecutionState::new(), source).await;
        let initial = state.snapshot_execution_state().expect("initial snapshot");
        assert_eq!(
            initial
                .components
                .values()
                .filter(|component| matches!(
                    component,
                    lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
                ))
                .count(),
            12
        );
        state.acknowledge_execution_state_capture();

        state = execute_test_code(
            state,
            "large_0 = push(large_0, \"one changed binding\")".to_string(),
        )
        .await;
        let changed = state.snapshot_execution_state().expect("changed snapshot");
        let changed_bodies = changed
            .components
            .values()
            .filter(|component| {
                matches!(
                    component,
                    lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
                )
            })
            .count();
        let unchanged_refs = changed
            .components
            .values()
            .filter(|component| {
                matches!(
                    component,
                    lash_core::plugin::ExecutionStateComponentSnapshot::Unchanged
                )
            })
            .count();
        assert_eq!(
            changed_bodies, 1,
            "only the assigned large binding carries bytes"
        );
        assert_eq!(unchanged_refs, 11, "all other large bindings ride as refs");

        let initial_budget = state::measure_snapshot(&initial);
        let changed_budget = state::measure_snapshot(&changed);
        assert_eq!(initial_budget.checkpoint_bytes, 82_515);
        assert_eq!(changed_budget.checkpoint_bytes, 14_033);
    });
}

#[test]
pub(super) fn progress_capture_then_later_assignment_survives_final_cold_reopen() {
    block_on(async {
        let initial_payload = format!("before-{}", "x".repeat(8 * 1024));
        let mut state = execute_test_code(
            RlmExecutionState::new(),
            format!("large = [\"{initial_payload}\"]"),
        )
        .await;
        let progress_snapshot = state
            .snapshot_execution_state()
            .expect("progress-boundary capture");

        state =
            execute_test_code(state, "large = push(large, \"after-progress\")".to_string()).await;
        let final_snapshot = state
            .snapshot_execution_state()
            .expect("final capture after later assignment");
        assert_ne!(
            final_snapshot.root, progress_snapshot.root,
            "the final capture must supersede the pending progress capture"
        );
        assert_eq!(
            final_snapshot
                .components
                .values()
                .filter(|component| matches!(
                    component,
                    lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
                ))
                .count(),
            1,
            "the post-progress value leaf must still carry its uncommitted body"
        );

        let hydrated = hydrate_snapshot(final_snapshot);
        let mut reopened = RlmExecutionState::new();
        reopened
            .restore_execution_state(&hydrated)
            .expect("cold reopen final capture");
        assert_eq!(
            reopened.rlm.snapshot().globals().get("large"),
            state.rlm.snapshot().globals().get("large"),
            "cold reopen must include the assignment made after the progress capture"
        );

        state.abort_execution_state_capture();
        let retry_snapshot = state
            .snapshot_execution_state()
            .expect("retry superseded capture after commit failure");
        let retry_hydrated = hydrate_snapshot(retry_snapshot);
        let mut retry_reopened = RlmExecutionState::new();
        retry_reopened
            .restore_execution_state(&retry_hydrated)
            .expect("cold reopen retry capture");
        assert_eq!(
            retry_reopened.rlm.snapshot().globals().get("large"),
            state.rlm.snapshot().globals().get("large"),
            "aborting a superseded capture must retain the post-progress assignment"
        );
    });
}

#[test]
pub(super) fn progress_capture_a_to_b_then_final_a_resends_the_evicted_leaf() {
    block_on(async {
        let payload_a = format!("a-{}", "x".repeat(8 * 1024));
        let payload_b = format!("b-{}", "y".repeat(8 * 1024));
        let mut state = execute_test_code(
            RlmExecutionState::new(),
            format!("large = [\"{payload_a}\"]"),
        )
        .await;
        let durable_a = state.snapshot_execution_state().expect("durable A capture");
        state.acknowledge_execution_state_capture();
        let mut staged_runtime = lash_core::RuntimeSessionState {
            session_id: SessionId::from("progress-a-b-a-staged"),
            ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
            ))
        };
        lash_core::testing::stage_execution_state_components(
            &mut staged_runtime,
            durable_a.clone(),
        )
        .expect("stage durable A");
        let mut retry_runtime = lash_core::RuntimeSessionState {
            session_id: SessionId::from("progress-a-b-a-retry"),
            ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
            ))
        };
        lash_core::testing::stage_execution_state_components(&mut retry_runtime, durable_a)
            .expect("stage retry baseline A");

        state = execute_test_code(state, format!("large = [\"{payload_b}\"]")).await;
        let progress_b = state
            .snapshot_execution_state()
            .expect("progress-boundary B capture");
        lash_core::testing::stage_execution_state_components(
            &mut staged_runtime,
            progress_b.clone(),
        )
        .expect("stage progress B");
        state = execute_test_code(state, format!("large = [\"{payload_a}\"]")).await;
        let final_a = state.snapshot_execution_state().expect("final A capture");
        assert_ne!(final_a.root, progress_b.root);
        assert_eq!(
            final_a
                .components
                .values()
                .filter(|component| matches!(
                    component,
                    lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
                ))
                .count(),
            1,
            "A was evicted by the staged B root, so final A must resend its body"
        );

        lash_core::testing::stage_execution_state_components(&mut staged_runtime, final_a)
            .expect("stage final A over progress B");
        let final_hydration = staged_runtime
            .execution_state_hydration()
            .expect("hydrate staged final A")
            .expect("final A root");
        let mut reopened = RlmExecutionState::new();
        reopened
            .restore_execution_state(&final_hydration)
            .expect("cold reopen final A capture");
        assert_eq!(
            reopened.rlm.snapshot().globals().get("large"),
            state.rlm.snapshot().globals().get("large")
        );

        state.abort_execution_state_capture();
        let retry_a = state
            .snapshot_execution_state()
            .expect("retry A after final commit failure");
        lash_core::testing::stage_execution_state_components(&mut retry_runtime, retry_a)
            .expect("stage retry A over durable A");
        let retry_hydration = retry_runtime
            .execution_state_hydration()
            .expect("hydrate retry A")
            .expect("retry A root");
        let mut retry_reopened = RlmExecutionState::new();
        retry_reopened
            .restore_execution_state(&retry_hydration)
            .expect("cold reopen retry A capture");
        assert_eq!(
            retry_reopened.rlm.snapshot().globals().get("large"),
            state.rlm.snapshot().globals().get("large")
        );
    });
}

#[test]
pub(super) fn measured_commit_growth_tracks_changed_state_not_session_size() {
    block_on(async {
        let mut source = String::new();
        for index in 0..16 {
            let payload = format!("session-{index}-{}", "y".repeat(8 * 1024));
            source.push_str(&format!("large_{index} = [\"{payload}\"]\n"));
        }
        for index in 0..80 {
            source.push_str(&format!("small_{index} = {index}\n"));
        }
        let mut state = execute_test_code(RlmExecutionState::new(), source).await;
        let full_state_bytes = state
            .rlm
            .snapshot()
            .to_canonical_bytes()
            .expect("pre-arc flat snapshot baseline")
            .len();
        let _initial = state.snapshot_execution_state().expect("initial snapshot");
        state.acknowledge_execution_state_capture();

        let mut measured = Vec::new();
        for turn in 0..40 {
            let binding = turn % 16;
            state = execute_test_code(
                state,
                format!("large_{binding} = push(large_{binding}, \"turn-{turn}\")"),
            )
            .await;
            let snapshot = state.snapshot_execution_state().expect("turn snapshot");
            assert_eq!(
                state.encoded_globals_in_last_snapshot(),
                1,
                "turn {turn} must re-encode only its assigned binding"
            );
            assert_eq!(
                snapshot
                    .components
                    .values()
                    .filter(|component| matches!(
                        component,
                        lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
                    ))
                    .count(),
                1,
                "turn {turn} must submit one changed leaf body"
            );
            measured.push(state::measure_snapshot(&snapshot).checkpoint_bytes);
            state.acknowledge_execution_state_capture();
        }
        let minimum = *measured.iter().min().expect("measurements");
        let maximum = *measured.iter().max().expect("measurements");
        println!(
            "FIG1195_FLAT_GROWTH full_state_bytes={full_state_bytes} min_commit_bytes={minimum} max_commit_bytes={maximum} turns={}",
            measured.len()
        );
        assert_eq!(full_state_bytes, 136_711);
        assert_eq!(minimum, 21_040);
        assert_eq!(maximum, 21_094);
    });
}

/// The failure geometry this arc exists for: a research session whose state
/// is many mid-size composite bindings rather than a few large ones. Three
/// live jitindex episodes committed 1.52/1.32/1.24 MB of exactly this shape
/// against a 1 MiB budget, so per-commit bytes have to track the changed
/// binding here too — a payoff that only appears above some large-binding
/// size would not have prevented those failures.
#[test]
pub(super) fn measured_commit_growth_stays_flat_for_many_mid_size_bindings() {
    block_on(async {
        let mut source = String::new();
        for index in 0..300 {
            let payload = format!("note-{index}-{}", "n".repeat(3 * 1024 + 512));
            source.push_str(&format!("mid_{index} = [\"{payload}\"]\n"));
        }
        let mut state = execute_test_code(RlmExecutionState::new(), source).await;
        let full_state_bytes = state
            .rlm
            .snapshot()
            .to_canonical_bytes()
            .expect("accumulated canonical state")
            .len();
        assert_eq!(full_state_bytes, 1_104_953);
        let _initial = state.snapshot_execution_state().expect("initial snapshot");
        state.acknowledge_execution_state_capture();

        let mut measured = Vec::new();
        for turn in 0..20 {
            let binding = turn % 300;
            state = execute_test_code(
                state,
                format!("mid_{binding} = push(mid_{binding}, \"turn-{turn}\")"),
            )
            .await;
            let snapshot = state.snapshot_execution_state().expect("turn snapshot");
            assert_eq!(
                state.encoded_globals_in_last_snapshot(),
                1,
                "turn {turn} must re-encode only its assigned binding"
            );
            assert_eq!(
                snapshot
                    .components
                    .values()
                    .filter(|component| matches!(
                        component,
                        lash_core::plugin::ExecutionStateComponentSnapshot::Changed(_)
                    ))
                    .count(),
                1,
                "turn {turn} must submit one changed leaf body"
            );
            measured.push(state::measure_snapshot(&snapshot).checkpoint_bytes);
            state.acknowledge_execution_state_capture();
        }
        let minimum = *measured.iter().min().expect("measurements");
        let maximum = *measured.iter().max().expect("measurements");
        println!(
            "FIG1195_FLAT_GROWTH_MID_SIZE full_state_bytes={full_state_bytes} min_commit_bytes={minimum} max_commit_bytes={maximum} turns={}",
            measured.len()
        );
        assert_eq!(minimum, 94_287);
        assert_eq!(maximum, 94_289);
    });
}

/// The other side of the leaf line: a session of many short bindings must
/// keep them inline. Each leaf costs a root reference plus a checkpoint
/// manifest row on every commit, so promoting short values to leaves would
/// raise the per-commit floor instead of lowering it.
#[test]
pub(super) fn many_short_bindings_stay_inline_and_hold_the_per_commit_floor() {
    block_on(async {
        let mut source = String::new();
        for index in 0..200 {
            let payload = format!("short-{index}-{}", "s".repeat(48));
            source.push_str(&format!("short_{index} = [\"{payload}\"]\n"));
        }
        let mut state = execute_test_code(RlmExecutionState::new(), source).await;
        let initial = state.snapshot_execution_state().expect("initial snapshot");
        assert_eq!(initial.components.len(), 0);
        state.acknowledge_execution_state_capture();

        state = execute_test_code(
            state,
            "short_0 = push(short_0, \"one changed binding\")".to_string(),
        )
        .await;
        let changed = state.snapshot_execution_state().expect("changed snapshot");
        let commit_bytes = state::measure_snapshot(&changed).checkpoint_bytes;
        println!(
            "FIG1195_SHORT_BINDING_FLOOR commit_bytes={commit_bytes} leaves={}",
            changed.components.len()
        );
        assert!(
            changed.components.is_empty(),
            "a changed short binding must not mint a leaf"
        );
        // The property under test is the assertion above: no leaf is minted,
        // so 200 short bindings cost no root references and no manifest
        // rows. The byte bound is a sanity ceiling on top of that. The
        // measurement is deterministic and has been 33,027 bytes since the
        // pre-heap tree representation — the heap form encodes the same
        // bytes for these bindings — so the ceiling is set well above it
        // rather than one percent above it: a tight assert here fails on any
        // harmless change to the payload strings while telling us nothing
        // the leaf-count assertion does not.
        assert!(
            commit_bytes < 48 * 1024,
            "many short bindings must keep the per-commit floor low: {commit_bytes}"
        );
    });
}

#[test]
pub(super) fn bound_variables_prompt_renders_live_globals_after_execution() {
    block_on(async {
        let mut state = RlmExecutionState::new();
        let ctx = lash_core::testing::code_execution_context();
        let response = execute_code_unbounded_for_tests(
            &mut state,
            ctx,
            ExecRequest {
                language: "lashlang".to_string(),
                code: "scratch_note = \"after execution\"".to_string(),
            },
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::new(
                lashlang::LashlangAbilities::default(),
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::new(),
            ),
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert_eq!(response.error, None);

        let globals = state.bound_variable_values(&BTreeSet::new());
        let mut cache = crate::rlm_support::BoundVariableRenderCache::default();
        let rendered = crate::rlm_support::render_bound_variables(
            &mut cache,
            &globals,
            crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
        );

        assert!(
            rendered.contains(r#"- `scratch_note` = "after execution""#),
            "{}",
            rendered
        );
    });
}

#[test]
pub(super) fn bound_variables_prompt_degrades_large_live_globals() {
    block_on(async {
        let mut state = RlmExecutionState::new();
        let ctx = lash_core::testing::code_execution_context();
        // Same constructs the runtime-perf `rlm_globals` scenario seeds:
        // a large record and a large list that exceed the inline budget.
        let code = "big_map = {}\n\
                for i in range(24) {\n\
                  big_map[format(\"room_{}\", i)] = { exits: [\"north\", \"south\"], items: [format(\"item_{}\", i)] }\n\
                }\n\
                big_notes = []\n\
                for i in range(45) {\n\
                  big_notes = push(big_notes, format(\"note {}: observation\", i))\n\
                }"
            .to_string();
        let response = execute_code_unbounded_for_tests(
            &mut state,
            ctx,
            ExecRequest {
                language: "lashlang".to_string(),
                code,
            },
            lashlang::global_in_memory_lashlang_artifact_store(),
            LashlangSurface::new(
                lashlang::LashlangAbilities::default(),
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::new(),
            ),
            None,
            RlmProjectedBindings::default(),
            Arc::new(ProjectionRegistry::new()),
            RlmLashlangExecutionTraceConfig::default(),
        )
        .await;
        assert_eq!(response.error, None);

        let globals = state.bound_variable_values(&BTreeSet::new());
        let mut cache = crate::rlm_support::BoundVariableRenderCache::default();
        let s = crate::rlm_support::render_bound_variables(
            &mut cache,
            &globals,
            crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
        )
        .to_string();

        // Large record -> type + keys=N + projector preview.
        assert!(s.contains("`big_map`:"), "{s}");
        assert!(s.contains("keys=24"), "{s}");
        assert!(s.contains("≈ {") && s.contains("room_0"), "{s}");
        assert!(s.contains("fields omitted"), "{s}");
        // Large list -> type + len=N + projector preview.
        assert!(s.contains("`big_notes`:"), "{s}");
        assert!(s.contains("len=45"), "{s}");
        assert!(s.contains("≈ [") && s.contains("note 0:"), "{s}");
        assert!(s.contains("items omitted"), "{s}");
    });
}

#[test]
pub(super) fn flow_to_json_value_emits_projected_marker_for_projected_values() {
    block_on(async {
        let projected = ProjectedValue::scalar("input", FlowValue::String("hello".into()));
        let value = flow_to_json_value(&FlowValue::Projected(projected)).await;
        let obj = value
            .as_object()
            .expect("expected projected wrapper object");
        assert_eq!(obj.len(), 1, "wrapper should have exactly one key");
        assert_eq!(
            obj.get(PROJECTED_JSON_TAG),
            Some(&serde_json::json!({
                "kind": "materialized",
                "value": "hello"
            }))
        );
    });
}

#[test]
pub(super) fn flow_to_json_value_preserves_projection_ref_without_materializing() {
    block_on(async {
        let host = Arc::new(SnapshotProjectedToolText::default());
        let reference = ProjectionRef::new("memory", serde_json::json!("doc"));
        let projected = ProjectedValue::custom_with_projection_ref(
            "doc",
            host.clone(),
            serde_json::json!(reference),
        );
        let value = flow_to_json_value(&FlowValue::Projected(projected)).await;
        assert_eq!(host.render_count.load(Ordering::SeqCst), 0);
        assert_eq!(host.materialize_count.load(Ordering::SeqCst), 0);
        assert_eq!(
            value,
            serde_json::json!({
                PROJECTED_JSON_TAG: {
                    "kind": "ref",
                    "value": {
                        "kind": "memory",
                        "key": "doc",
                    }
                }
            })
        );
    });
}

#[test]
pub(super) fn flow_to_json_value_materializes_an_invalid_projection_ref() {
    block_on(async {
        let host = Arc::new(SnapshotProjectedToolText::default());
        let projected = ProjectedValue::custom_with_projection_ref(
            "doc",
            host.clone(),
            serde_json::Value::Null,
        );
        let value = flow_to_json_value(&FlowValue::Projected(projected)).await;
        assert_eq!(host.materialize_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            value,
            serde_json::json!({
                PROJECTED_JSON_TAG: {
                    "kind": "materialized",
                    "value": "materialized tool text",
                }
            })
        );
    });
}

#[test]
pub(super) fn image_json_round_trip_preserves_mime_and_image_type() {
    block_on(async {
        let image = lashlang::ImageValue::new(
            "image-sha256",
            lash_core::MediaType::parse("image/webp").unwrap(),
            "cover",
            73,
            Some(320),
            Some(180),
        );
        let flow = FlowValue::Image(Box::new(image));
        let json = flow_to_json_value(&flow).await;

        assert_eq!(json.get("mime").and_then(Value::as_str), Some("image/webp"));
        assert!(json.get("media_type").is_none());
        assert_eq!(json_to_flow_value(json), flow);
    });
}

#[test]
pub(super) fn executor_snapshot_round_trips_projection_ref_metadata() {
    let reference = ProjectionRef::new("memory", serde_json::json!("doc"));
    let mut state = RlmExecutionState::new();
    state
        .rlm
        .insert_global(
            "doc".to_string(),
            FlowValue::Projected(ProjectedValue::custom_with_projection_ref(
                "doc",
                Arc::new(SnapshotProjectedToolText::default()),
                serde_json::json!(reference),
            )),
        )
        .expect("insert projected global");

    let snapshot = hydrate_snapshot(state.snapshot_execution_state().expect("executor snapshot"));

    let mut restored_execution = RlmExecutionState::new();
    restored_execution
        .restore_execution_state(&snapshot)
        .expect("restore runtime");
    let restored = restored_execution.rlm;
    let restored_snapshot = restored.snapshot();
    let Some(FlowValue::Projected(projected)) = restored_snapshot.globals().get("doc") else {
        panic!("expected restored projected value");
    };
    assert_eq!(
        projected.projection_ref(),
        Some(&serde_json::json!({"kind": "memory", "key": "doc"}))
    );
}

#[test]
pub(super) fn flow_record_to_json_value_marks_only_projected_entries() {
    block_on(async {
        let projected = ProjectedValue::scalar("input", FlowValue::String("p".into()));
        let mut record = FlowRecord::default();
        record.insert("proj".to_string(), FlowValue::Projected(projected));
        record.insert("glob".to_string(), FlowValue::String("g".into()));

        let value = flow_record_to_json_value(&record).await;
        let obj = value.as_object().expect("record object");
        // proj entry must be wrapped in {"__projected__": ...}
        let proj = obj
            .get("proj")
            .and_then(|v| v.as_object())
            .expect("proj entry is an object");
        assert!(proj.contains_key(PROJECTED_JSON_TAG));
        // glob entry stays a bare string
        assert_eq!(obj.get("glob").and_then(|v| v.as_str()).expect("glob"), "g");
    });
}

#[test]
pub(super) fn flow_record_to_tool_args_materializes_ordinary_tools() {
    block_on(async {
        let projected = ProjectedValue::scalar("input", FlowValue::String("p".into()));
        let mut record = FlowRecord::default();
        record.insert("query".to_string(), FlowValue::Projected(projected));

        let value = flow_record_to_tool_args(
            &record,
            &lash_core::ToolArgumentProjectionPolicy::MaterializeProjectedValues,
        )
        .await
        .expect("projection transport should be canonical");

        assert_eq!(value, serde_json::json!({ "query": "p" }));
    });
}

#[test]
pub(super) fn flow_record_to_tool_args_preserves_only_seed_projected_roots() {
    block_on(async {
        let reference = ProjectionRef::new("memory", serde_json::json!("doc"));
        let projected_root = ProjectedValue::custom_with_projection_ref(
            "doc",
            Arc::new(SnapshotProjectedToolText::default()),
            serde_json::json!(reference),
        );
        let mut computed = FlowRecord::default();
        computed.insert(
            "summary".to_string(),
            FlowValue::Projected(ProjectedValue::scalar(
                "summary",
                FlowValue::String("materialized summary".into()),
            )),
        );
        let mut seed = FlowRecord::default();
        seed.insert("problem".to_string(), FlowValue::Projected(projected_root));
        seed.insert(
            "computed".to_string(),
            FlowValue::Record(Arc::new(computed)),
        );
        let mut record = FlowRecord::default();
        record.insert(
            "task".to_string(),
            FlowValue::Projected(ProjectedValue::scalar(
                "task",
                FlowValue::String("inspect".into()),
            )),
        );
        record.insert("seed".to_string(), FlowValue::Record(Arc::new(seed)));

        let value = flow_record_to_tool_args(
            &record,
            &lash_core::ToolArgumentProjectionPolicy::preserve_projected_refs_in_field("seed"),
        )
        .await
        .expect("projection transport should be canonical");

        assert_eq!(
            value,
            serde_json::json!({
                "task": "inspect",
                "seed": {
                    "problem": {
                        "__projected__": {
                            "kind": "ref",
                            "value": {
                                "kind": "memory",
                                "key": "doc"
                            }
                        }
                    },
                    "computed": {
                        "summary": "materialized summary"
                    }
                }
            })
        );
    });
}
