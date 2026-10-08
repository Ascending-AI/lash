use super::*;

use lashlang::testing::ast_builders as b;

struct GrowthMeasurement {
    started: std::time::Instant,
    pool: lash_vm_client::WorkerPool,
}

impl Drop for GrowthMeasurement {
    #[expect(
        clippy::disallowed_methods,
        reason = "record host contention in the growth witness"
    )]
    fn drop(&mut self) {
        eprintln!(
            "FIG4474_GROWTH elapsed_ms={:.3} worker_cpu_ms={:.3} panicking={} pool={:?} loadavg={}",
            self.started.elapsed().as_secs_f64() * 1000.0,
            self.pool.measured_cpu().as_secs_f64() * 1000.0,
            std::thread::panicking(),
            self.pool.stats(),
            std::fs::read_to_string("/proc/loadavg")
                .unwrap_or_else(|_| "unavailable".into())
                .trim(),
        );
    }
}

/// `finish { <name>: <expr>, .. }` — the shape every projection witness reads
/// its bindings back with. ADR 0096 retired the Lashlang front-end, so these
/// cells state their AST; the source each stood for is kept at the call site.
fn finish_record(fields: &[(&str, lashlang::Expr)]) -> lashlang::Program {
    b::program(vec![b::finish(b::record(
        fields
            .iter()
            .map(|(name, expr)| (*name, expr.clone()))
            .collect(),
    ))])
}

/// `seed = [{ nested: [1] }]`
fn seed_nested_one() -> lashlang::Program {
    b::program(vec![b::assign(
        "seed",
        b::list(vec![b::record(vec![(
            "nested",
            b::list(vec![b::num(1.0)]),
        )])]),
    )])
}

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
            .await
            .expect("patch diary");

        let projected = projected_history(vec![FlowValue::String("hello".into())]);
        let compiled = worker_compile_program(&finish_record(&[
            ("history_len", b::builtin("len", vec![b::var("history")])),
            ("diary_len", b::builtin("len", vec![b::var("diary")])),
        ]))
        .await
        .expect("compile");
        let outcome = execute_with_projected(&compiled, state.vm.state_mut(), &projected)
            .await
            .expect("execute");
        let ExecutionOutcome::Finished(FlowValue::Record(record)) = outcome else {
            panic!("expected finishted record");
        };
        assert_eq!(record["history_len"], FlowValue::Number(1.0));
        assert_eq!(record["diary_len"], FlowValue::Number(1.0));
        assert!(state.vm.state().globals().get("history").is_none());
    });
}

#[tokio::test]
pub(super) async fn set_default_initializes_once_and_does_not_mutate_projected_globals() {
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
        .await
        .expect("apply defaults");
    assert_eq!(
        state.vm.state().globals().get("diary"),
        Some(&FlowValue::List(
            vec![FlowValue::String("initial".into())].into()
        ))
    );
    assert!(state.vm.state().globals().get("current_query").is_none());

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
        .await
        .expect("reapply defaults");
    assert_eq!(
        state.vm.state().globals().get("diary"),
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
        let setup = worker_compile_program(&seed_nested_one())
            .await
            .expect("compile setup");
        execute_with_projected(&setup, state.vm.state_mut(), &projected)
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
            .await
            .expect("patch heap-backed state");

        let finish = worker_compile_program(&b::program(vec![b::finish(b::var("diary"))]))
            .await
            .expect("compile finish");
        assert_eq!(
            execute_with_projected(&finish, state.vm.state_mut(), &projected)
                .await
                .expect("next cell sees patch"),
            ExecutionOutcome::Finished(FlowValue::List(
                vec![FlowValue::String("kept".into())].into()
            ))
        );

        let bytes = state
            .vm
            .state()
            .bytes()
            .expect("canonical worker state")
            .to_vec();
        let snapshot = lashlang::VmInstance::pristine()
            .open_snapshot(&bytes)
            .expect("decode patched state");
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
        let setup = worker_compile_program(&seed_nested_one())
            .await
            .expect("compile setup");
        execute_with_projected(&setup, state.vm.state_mut(), &projected)
            .await
            .expect("execute setup");
        let before = state
            .vm
            .state()
            .bytes()
            .expect("canonical worker state")
            .to_vec();
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
            .await
            .expect_err("a reserved name must reject the whole patch");
        assert!(error.to_string().contains("history"));

        assert!(
            state.vm.state().globals().get("a_good").is_none(),
            "no key from a rejected patch may be committed"
        );
        assert_eq!(
            state.vm.state().bytes().expect("canonical worker state"),
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
        let setup = worker_compile_program(&b::program(vec![b::assign(
            "seed",
            b::list(vec![b::num(1.0)]),
        )]))
        .await
        .expect("compile setup");
        execute_with_projected(&setup, state.vm.state_mut(), &projected)
            .await
            .expect("execute setup");
        let before = state
            .vm
            .state()
            .bytes()
            .expect("canonical worker state")
            .to_vec();

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
            .await
            .expect_err("a protected name must reject the whole patch");

        assert_eq!(
            state.vm.state().bytes().expect("canonical worker state"),
            before
        );
    });
}

#[test]
pub(super) fn heap_backed_projection_and_prune_survive_execution_and_restore() {
    block_on(async {
        // history = [{ role: "user" }]
        // kept = [{ nested: [2] }]
        let setup = worker_compile_program(&b::program(vec![
            b::assign(
                "history",
                b::list(vec![b::record(vec![("role", b::string("user"))])]),
            ),
            b::assign(
                "kept",
                b::list(vec![b::record(vec![(
                    "nested",
                    b::list(vec![b::num(2.0)]),
                )])]),
            ),
        ]))
        .await
        .expect("compile setup");
        let mut state = lashlang::State::new();
        execute_with_projected(&setup, &mut state, &ProjectedBindings::new())
            .await
            .expect("execute setup");

        state
            .insert_global(
                "doc",
                FlowValue::Projected(lashlang::testing::projection::test_view(
                    "doc",
                    Arc::new(SnapshotProjectedToolText::default()),
                )),
            )
            .expect("insert projected value");
        let bytes = state
            .snapshot()
            .to_canonical_bytes()
            .expect("encode projected state");
        state = lashlang::State::from_snapshot(
            lashlang::VmInstance::pristine()
                .open_snapshot(&bytes)
                .expect("restore projected state"),
        );
        // `doc` restores as the same plain-data projection (ADR 0132 §9); a
        // same-named projected binding takes its read-only slot at the next
        // execution.
        let mut projected = ProjectedBindings::new();
        projected.insert(
            "doc",
            lashlang::testing::projection::test_view(
                "doc",
                Arc::new(SnapshotProjectedToolText::default()),
            ),
        );
        crate::projection::prune_reserved_projected_bindings(&mut state);

        let finish = worker_compile_program(&finish_record(&[
            ("doc", b::var("doc")),
            ("kept", b::var("kept")),
        ]))
        .await
        .expect("compile post-patch read");
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
                .expect("next cell sees the re-supplied projection and prune"),
            expected
        );
        assert!(state.globals().get("history").is_none());

        let bytes = state
            .snapshot()
            .to_canonical_bytes()
            .expect("encode refreshed and pruned state");
        let snapshot = lashlang::VmInstance::pristine()
            .open_snapshot(&bytes)
            .expect("decode refreshed and pruned state");
        let mut restored = lashlang::State::from_snapshot(snapshot);
        assert_eq!(
            execute_with_projected(&finish, &mut restored, &projected)
                .await
                .expect("cold-restored cell sees the re-supplied projection and prune"),
            expected
        );
        assert!(restored.globals().get("history").is_none());
    });
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

        let compiled = worker_compile_program(&finish_record(&[
            ("chars", b::builtin("len", vec![b::var("current_query")])),
            ("value", b::var("current_query")),
        ]))
        .await
        .expect("compile read");
        let outcome = execute_with_projected(&compiled, state.vm.state_mut(), &projected)
            .await
            .expect("execute read");
        let ExecutionOutcome::Finished(FlowValue::Record(record)) = outcome else {
            panic!("expected finishted record");
        };
        assert_eq!(record["chars"], FlowValue::Number(4.0));
        assert_eq!(record["value"], FlowValue::String("host".into()));
        assert!(state.vm.state().globals().get("current_query").is_none());

        let compiled = worker_compile_program(&b::program(vec![b::assign(
            "current_query",
            b::string("local"),
        )]))
        .await
        .expect("compile write");
        let env = ExecutionEnvironment::new(&NoopHost)
            .traced()
            .with_projected_bindings(projected.clone());
        let error = execute_with_projected(&compiled, state.vm.state_mut(), &projected)
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

#[tokio::test]
pub(super) async fn executor_snapshot_does_not_materialize_projected_tool_result_globals() {
    let projected = Arc::new(SnapshotProjectedToolText::default());
    let mut state = RlmExecutionState::new();
    state
        .vm
        .state_mut()
        .insert_global(
            "m".to_string(),
            FlowValue::Projected(lashlang::testing::projection::test_view(
                "search.matches[0].text",
                projected.clone(),
            )),
        )
        .await
        .expect("insert projected global");

    let snapshot = hydrate_snapshot(
        state
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
            .expect("executor snapshot"),
    );
    assert_eq!(projected.render_count.load(Ordering::SeqCst), 0);
    assert_eq!(projected.materialize_count.load(Ordering::SeqCst), 0);
    let mut encoded = snapshot.root.to_vec();
    for body in snapshot.components.values() {
        encoded.extend_from_slice(body);
    }
    let encoded_text = String::from_utf8_lossy(&encoded);
    assert!(!encoded_text.contains("rendered tool text"));
    assert!(!encoded_text.contains("materialized tool text"));

    let mut restored_execution = RlmExecutionState::new();
    restored_execution
        .restore_execution_state(&snapshot, lash_core::FleetFormat::current())
        .await
        .expect("restore runtime");
    let restored = restored_execution.vm.state().clone();
    assert!(matches!(
        restored.globals().get("m"),
        Some(FlowValue::Projected(_))
    ));
}

#[test]
pub(super) fn flow_to_json_value_materializes_a_custom_projection() {
    lashlang::testing::projection::with_test_views(|| {
        block_on(async {
            let host = Arc::new(SnapshotProjectedToolText::default());
            let projected = lashlang::testing::projection::test_view("doc", host.clone());
            let value = flow_to_json_value(&FlowValue::Projected(projected));
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
        })
    });
}

#[test]
pub(super) fn flow_record_to_tool_args_preserves_only_seed_projected_roots() {
    lashlang::testing::projection::with_test_views(|| {
        block_on(async {
            let projected_root = lashlang::testing::projection::test_view(
                "doc",
                Arc::new(SnapshotProjectedToolText::default()),
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
                                "kind": "materialized",
                                "value": "materialized tool text"
                            }
                        },
                        "computed": {
                            "summary": "materialized summary"
                        }
                    }
                })
            );
        })
    });
}

/// Run `code` as one cell of a session whose execution state is `state`, on
/// a durable host of its own; the cell must succeed.
pub(super) async fn execute_test_code(
    mut state: RlmExecutionState,
    code: String,
) -> RlmExecutionState {
    let handler = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
    let response = Box::pin(execute_code_with_test_render(
        &mut state,
        lash_core::testing::code_execution_context(handler.ports()),
        ExecRequest { code },
        handler.artifacts(),
        LashlangSurface::default(),
        None,
        RlmProjectedBindings::default(),
        None,
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    ))
    .await;
    assert_eq!(response.error, None, "test TypeScript execution failed");
    state
}

/// The TypeScript frontend caps a single cell at 64 KiB of source, which the
/// state-growth fixtures below deliberately exceed: they seed dozens of
/// multi-kilobyte bindings before measuring what one later assignment costs.
/// Seeding them one cell at a time is the same end state — RLM globals persist
/// across cells — without pretending a model would ever emit a 1 MB cell.
pub(super) async fn execute_test_code_chunked(
    mut state: RlmExecutionState,
    source: String,
) -> RlmExecutionState {
    const MAX_CELL_BYTES: usize = 48 * 1024;
    let mut cell = String::new();
    for line in source.lines() {
        if !cell.is_empty() && cell.len() + line.len() + 1 > MAX_CELL_BYTES {
            state = Box::pin(execute_test_code(state, std::mem::take(&mut cell))).await;
        }
        cell.push_str(line);
        cell.push('\n');
    }
    if !cell.trim().is_empty() {
        state = Box::pin(execute_test_code(state, cell)).await;
    }
    state
}

#[test]
pub(super) fn measured_commit_budget_carries_only_changed_leaf_bodies() {
    block_on(async {
        let mut source = String::new();
        for index in 0..12 {
            let payload = format!("large-{index}-{}", "x".repeat(6 * 1024));
            source.push_str(&format!("let large_{index} = [\"{payload}\"];\n"));
        }
        for index in 0..40 {
            source.push_str(&format!("let small_{index} = {index};\n"));
        }
        let mut state = execute_test_code_chunked(RlmExecutionState::new(), source).await;
        let initial = state
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
            .expect("initial snapshot");
        assert_eq!(
            initial
                .leaves()
                .values()
                .filter(|component| matches!(component, lash_core::plugin::LeafChange::Changed(_)))
                .count(),
            12
        );
        state.acknowledge_execution_state_capture();

        state = execute_test_code(
            state,
            // A TypeScript cell cannot assign to a prior cell's binding — an
            // ambient global is `const` — so "change one binding" is a
            // re-declaration that carries the same payload plus the new entry.
            format!(
                "let large_0 = [\"large-0-{}\", \"one changed binding\"];",
                "x".repeat(6 * 1024)
            ),
        )
        .await;
        let changed = state
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
            .expect("changed snapshot");
        let changed_bodies = changed
            .leaves()
            .values()
            .filter(|component| matches!(component, lash_core::plugin::LeafChange::Changed(_)))
            .count();
        let unchanged_refs = changed
            .leaves()
            .values()
            .filter(|component| matches!(component, lash_core::plugin::LeafChange::Unchanged))
            .count();
        assert_eq!(
            changed_bodies, 1,
            "only the assigned large binding carries bytes"
        );
        assert_eq!(unchanged_refs, 11, "all other large bindings ride as refs");

        let initial_budget = state::measure_snapshot(&initial);
        let changed_budget = state::measure_snapshot(&changed);
        println!(
            "RLM_SNAPSHOT_BUDGET initial={} changed={}",
            initial_budget.checkpoint_bytes, changed_budget.checkpoint_bytes
        );
        // Pin serializer measurements for the counter-only header and list-owned holes.
        assert_eq!(initial_budget.checkpoint_bytes, 81_846);
        assert_eq!(changed_budget.checkpoint_bytes, 13_199);
    });
}

#[test]
pub(super) fn progress_capture_then_later_assignment_survives_final_cold_reopen() {
    block_on(async {
        let initial_payload = format!("before-{}", "x".repeat(8 * 1024));
        let mut state = execute_test_code(
            RlmExecutionState::new(),
            format!("let large = [\"{initial_payload}\"];"),
        )
        .await;
        let progress_snapshot = state
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
            .expect("progress-boundary capture");

        state = execute_test_code(
            state,
            format!("let large = [\"{initial_payload}\", \"after-progress\"];"),
        )
        .await;
        let final_snapshot = state
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
            .expect("final capture after later assignment");
        assert_ne!(
            final_snapshot.root(),
            progress_snapshot.root(),
            "the final capture must supersede the pending progress capture"
        );
        assert_eq!(
            final_snapshot
                .leaves()
                .values()
                .filter(|component| matches!(component, lash_core::plugin::LeafChange::Changed(_)))
                .count(),
            1,
            "the post-progress value leaf must still carry its uncommitted body"
        );

        let hydrated = hydrate_snapshot(final_snapshot);
        let mut reopened = RlmExecutionState::new();
        reopened
            .restore_execution_state(&hydrated, lash_core::FleetFormat::current())
            .await
            .expect("cold reopen final capture");
        assert_eq!(
            reopened.vm.state().globals().get("large"),
            state.vm.state().globals().get("large"),
            "cold reopen must include the assignment made after the progress capture"
        );

        state.abort_execution_state_capture();
        let retry_snapshot = state
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
            .expect("retry superseded capture after commit failure");
        let retry_hydrated = hydrate_snapshot(retry_snapshot);
        let mut retry_reopened = RlmExecutionState::new();
        retry_reopened
            .restore_execution_state(&retry_hydrated, lash_core::FleetFormat::current())
            .await
            .expect("cold reopen retry capture");
        assert_eq!(
            retry_reopened.vm.state().globals().get("large"),
            state.vm.state().globals().get("large"),
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
            format!("let large = [\"{payload_a}\"];"),
        )
        .await;
        let durable_a = state
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
            .expect("durable A capture");
        state.acknowledge_execution_state_capture();
        let mut staged_runtime = lash_core::RuntimeSessionState {
            session_id: lash_core::SessionId::from("progress-a-b-a-staged"),
            ..lash_core::RuntimeSessionState::ambient_fixture(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ))
        };
        lash_core::testing::stage_execution_state_components(
            &mut staged_runtime,
            durable_a.clone(),
        )
        .expect("stage durable A");
        let mut retry_runtime = lash_core::RuntimeSessionState {
            session_id: lash_core::SessionId::from("progress-a-b-a-retry"),
            ..lash_core::RuntimeSessionState::ambient_fixture(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ))
        };
        lash_core::testing::stage_execution_state_components(&mut retry_runtime, durable_a)
            .expect("stage retry baseline A");

        state = execute_test_code(state, format!("let large = [\"{payload_b}\"];")).await;
        let progress_b = state
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
            .expect("progress-boundary B capture");
        lash_core::testing::stage_execution_state_components(
            &mut staged_runtime,
            progress_b.clone(),
        )
        .expect("stage progress B");
        state = execute_test_code(state, format!("let large = [\"{payload_a}\"];")).await;
        let final_a = state
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
            .expect("final A capture");
        assert_ne!(final_a.root(), progress_b.root());
        assert_eq!(
            final_a
                .leaves()
                .values()
                .filter(|component| matches!(component, lash_core::plugin::LeafChange::Changed(_)))
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
            .restore_execution_state(&final_hydration, lash_core::FleetFormat::current())
            .await
            .expect("cold reopen final A capture");
        assert_eq!(
            reopened.vm.state().globals().get("large"),
            state.vm.state().globals().get("large")
        );

        state.abort_execution_state_capture();
        let retry_a = state
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
            .expect("retry A after final commit failure");
        lash_core::testing::stage_execution_state_components(&mut retry_runtime, retry_a)
            .expect("stage retry A over durable A");
        let retry_hydration = retry_runtime
            .execution_state_hydration()
            .expect("hydrate retry A")
            .expect("retry A root");
        let mut retry_reopened = RlmExecutionState::new();
        retry_reopened
            .restore_execution_state(&retry_hydration, lash_core::FleetFormat::current())
            .await
            .expect("cold reopen retry A capture");
        assert_eq!(
            retry_reopened.vm.state().globals().get("large"),
            state.vm.state().globals().get("large")
        );
    });
}

#[test]
pub(super) fn measured_commit_growth_tracks_changed_state_not_session_size() {
    block_on(async {
        let mut source = String::new();
        for index in 0..16 {
            let payload = format!("session-{index}-{}", "y".repeat(8 * 1024));
            source.push_str(&format!("let large_{index} = [\"{payload}\"];\n"));
        }
        for index in 0..80 {
            source.push_str(&format!("let small_{index} = {index};\n"));
        }
        let mut state = execute_test_code_chunked(RlmExecutionState::new(), source).await;
        let full_state_bytes = state
            .vm
            .state()
            .bytes()
            .expect("canonical worker state")
            .len();
        let _initial = state
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
            .expect("initial snapshot");
        state.acknowledge_execution_state_capture();

        let mut measured = Vec::new();
        for turn in 0..40 {
            let binding = turn % 16;
            state = execute_test_code(
                state,
                format!(
                    "let large_{binding} = [\"session-{binding}-{}\", \"turn-{turn}\"];",
                    "y".repeat(8 * 1024)
                ),
            )
            .await;
            let snapshot = state
                .snapshot_execution_state(lash_core::FleetFormat::current())
                .await
                .expect("turn snapshot");
            assert_eq!(
                state.encoded_globals_in_last_snapshot(),
                1,
                "turn {turn} must re-encode only its assigned binding"
            );
            assert_eq!(
                snapshot
                    .leaves()
                    .values()
                    .filter(|component| matches!(
                        component,
                        lash_core::plugin::LeafChange::Changed(_)
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
        assert_eq!(full_state_bytes, 136_767);
        // Pin serializer measurements for the counter-only header and list-owned holes.
        assert_eq!(minimum, 19_366);
        assert_eq!(maximum, 19_368);
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
        let started = std::time::Instant::now();
        let state = RlmExecutionState::new();
        let pool = state
            .vm
            .state()
            .service()
            .pool()
            .expect("witness worker pool");
        let measurement = GrowthMeasurement { started, pool };
        let mut source = String::new();
        for index in 0..300 {
            let payload = format!("note-{index}-{}", "n".repeat(3 * 1024 + 512));
            source.push_str(&format!("let mid_{index} = [\"{payload}\"];\n"));
        }
        let mut state = execute_test_code_chunked(state, source).await;
        eprintln!(
            "FIG4474_SEED elapsed_ms={:.3} worker_cpu_ms={:.3}",
            measurement.started.elapsed().as_secs_f64() * 1000.0,
            measurement.pool.measured_cpu().as_secs_f64() * 1000.0
        );
        let full_state_bytes = state
            .vm
            .state()
            .bytes()
            .expect("canonical worker state")
            .len();
        assert_eq!(full_state_bytes, 1_106_995);
        let _initial = state
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
            .expect("initial snapshot");
        state.acknowledge_execution_state_capture();

        let mut measured = Vec::new();
        for turn in 0..20 {
            let binding = turn % 300;
            state = execute_test_code(
                state,
                format!(
                    "let mid_{binding} = [\"note-{binding}-{}\", \"turn-{turn}\"];",
                    "n".repeat(3 * 1024 + 512)
                ),
            )
            .await;
            let snapshot = state
                .snapshot_execution_state(lash_core::FleetFormat::current())
                .await
                .expect("turn snapshot");
            assert_eq!(
                state.encoded_globals_in_last_snapshot(),
                1,
                "turn {turn} must re-encode only its assigned binding"
            );
            assert_eq!(
                snapshot
                    .leaves()
                    .values()
                    .filter(|component| matches!(
                        component,
                        lash_core::plugin::LeafChange::Changed(_)
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
        // Pin serializer measurements for the counter-only header and list-owned holes.
        assert_eq!(minimum, 94_299);
        assert_eq!(maximum, 94_301);
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
            source.push_str(&format!("let short_{index} = [\"{payload}\"];\n"));
        }
        let mut state = execute_test_code_chunked(RlmExecutionState::new(), source).await;
        let initial = state
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
            .expect("initial snapshot");
        assert_eq!(initial.leaves().len(), 0);
        state.acknowledge_execution_state_capture();

        state = execute_test_code(
            state,
            format!(
                "let short_0 = [\"short-0-{}\", \"one changed binding\"];",
                "s".repeat(48)
            ),
        )
        .await;
        let changed = state
            .snapshot_execution_state(lash_core::FleetFormat::current())
            .await
            .expect("changed snapshot");
        let commit_bytes = state::measure_snapshot(&changed).checkpoint_bytes;
        println!(
            "FIG1195_SHORT_BINDING_FLOOR commit_bytes={commit_bytes} leaves={}",
            changed.leaves().len()
        );
        assert!(
            changed.leaves().is_empty(),
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
pub(super) fn bound_variables_prompt_degrades_large_live_globals() {
    block_on(async {
        let mut state = RlmExecutionState::new();
        let handler = crate::testing::DurableHost::open(crate::testing::default_cell_scope()).await;
        let ctx = lash_core::testing::code_execution_context(handler.ports());
        // Same constructs the runtime-perf `rlm_globals` scenario seeds:
        // a large record and a large list that exceed the inline budget.
        let code = r#"let big_map: Record<string, unknown> = {};
                for (let i = 0; i < 24; i++) {
                  big_map[`room_${i}`] = { exits: ["north", "south"], items: [`item_${i}`] };
                }
                let big_notes: string[] = [];
                for (let i = 0; i < 45; i++) {
                  big_notes = [...big_notes, `note ${i}: observation`];
                }"#
        .to_string();
        let response = execute_code_with_test_render(
            &mut state,
            ctx,
            ExecRequest { code },
            handler.artifacts(),
            LashlangSurface::new(
                lashlang::LashlangLanguageFeatures::default(),
                lashlang::LashlangHostCatalog::new(),
            ),
            None,
            RlmProjectedBindings::default(),
            None,
            lashlang::ExecutionBounds::unbounded(),
            crate::plugin::RlmChannel::Cell,
        )
        .await;
        assert_eq!(response.error, None);

        let globals = state.bound_variable_values(&BTreeSet::new());
        let mut cache = crate::rlm_support::BoundVariableRenderCache::default();
        let s = crate::rlm_support::render_bound_variables(
            &mut cache,
            &globals,
            &[],
            &crate::dialect::TypescriptDialect,
            &crate::render::BuiltinCodeRenderer,
            &lash_render::RenderParams::preview(),
            crate::RlmPresentationConfig::standard().max_inline_keys,
        )
        .to_string();

        // Large record -> type + keys=N + projector preview.
        assert!(s.contains("`big_map`:"), "{s}");
        assert!(s.contains("keys=24"), "{s}");
        assert!(s.contains("≈ {") && s.contains("room_0"), "{s}");
        assert!(s.contains("≈ {"), "{s}");
        // Large list -> type + len=N + projector preview.
        assert!(s.contains("`big_notes`:"), "{s}");
        assert!(s.contains("len=45"), "{s}");
        assert!(s.contains("≈ [") && s.contains("note 0:"), "{s}");
        assert!(s.contains("hidden items"), "{s}");
    });
}

/// A member read of a projected scalar reaches a tool as its plain value, even
/// in the field whose policy carries projections across as handles; only the
/// unread binding crosses as one (FIG-5197).
#[test]
pub(super) fn a_projected_scalar_read_reaches_a_tool_as_its_plain_value() {
    block_on(async {
        let definition = crate::continue_as_tool_definition(&crate::dialect::TypescriptDialect);
        let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![definition]);
        let invocation = lash_core::testing::exec_code_invocation(
            "test-session",
            "turn-7",
            7,
            2,
            "exec-code-3",
            "exec-code:3",
        );
        let handler = crate::testing::DurableHost::open(lash_core::AdmittedScope::turn(
            lash_core::SessionId::from("test-session"),
            lash_core::TurnId::from("turn-7"),
        ))
        .await;
        let context =
            lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
                handler.ports(),
                Arc::new(crate::control_tools::RlmControlToolsProvider {
                    vocabulary: crate::dialect::Dialect::prompt_vocabulary(
                        &crate::dialect::TypescriptDialect,
                    ),
                }),
                catalog,
                invocation,
            );
        let bindings = RlmProjectedBindings::new()
            .bind_json("report", serde_json::json!({ "title": "q3" }))
            .expect("bind report");
        let response = execute_code_with_test_render(
            &mut RlmExecutionState::new(),
            context,
            ExecRequest {
                code: "await control.continue_as({ task: report.title, seed: { title: report.title, doc: report } });"
                    .to_string(),
            },
            handler.artifacts(),
            LashlangSurface::default(),
            None,
            bindings,
            None,
            lashlang::ExecutionBounds::unbounded(),
            crate::plugin::RlmChannel::Cell,
        )
        .await;
        assert_eq!(response.error, None);
        let record = response
            .tool_calls
            .into_iter()
            .next()
            .expect("one continue_as host record");
        assert_eq!(record.args["task"], serde_json::json!("q3"));
        assert_eq!(
            record.args["seed"],
            serde_json::json!({
                "title": "q3",
                "doc": {
                    PROJECTED_JSON_TAG: { "kind": "materialized", "value": { "title": "q3" } }
                }
            })
        );
    });
}
