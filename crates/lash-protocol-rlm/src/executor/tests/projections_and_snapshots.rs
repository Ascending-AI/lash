use super::*;

use lashlang::testing::ast_builders as b;

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
pub(super) fn heap_backed_projection_refresh_and_prune_survive_execution_and_restore() {
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
                FlowValue::Projected(ProjectedValue::custom(
                    "doc",
                    Arc::new(SnapshotProjectedToolText::default()),
                )),
            )
            .expect("insert projected value");
        let unavailable = state
            .snapshot()
            .to_canonical_bytes()
            .expect("encode projected state");
        state = lashlang::State::from_snapshot(
            lashlang::VmInstance::pristine()
                .open_snapshot(&unavailable)
                .expect("restore projected state as unavailable"),
        );
        // The restore left `doc` a placeholder; a same-named projected
        // binding re-supplies it in place at the next execution.
        let mut projected = ProjectedBindings::new();
        projected.insert(
            "doc",
            ProjectedValue::custom("doc", Arc::new(SnapshotProjectedToolText::default())),
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
            FlowValue::Projected(ProjectedValue::custom(
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
    block_on(async {
        let host = Arc::new(SnapshotProjectedToolText::default());
        let projected = ProjectedValue::custom("doc", host.clone());
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
    });
}

#[test]
pub(super) fn flow_record_to_tool_args_preserves_only_seed_projected_roots() {
    block_on(async {
        let projected_root =
            ProjectedValue::custom("doc", Arc::new(SnapshotProjectedToolText::default()));
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
    });
}

/// An exported descriptor cannot be replaced by its materialized value: this
/// oracle answers Render and Materialize differently. The parked cell keeps
/// the live host registry and completes on the build that admitted it.
#[tokio::test]
pub(super) async fn exported_host_descriptor_declines_handover_without_losing_its_reads() {
    struct DescriptorHost<'a> {
        descriptor: Arc<SnapshotProjectedToolText>,
        parked: &'a tokio::sync::Notify,
        release: &'a tokio::sync::Notify,
        rendered: &'a std::sync::Mutex<Vec<String>>,
    }
    impl ExecutionHost for DescriptorHost<'_> {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
            match op {
                AbilityOp::ResourceOperation(_) => Ok(AbilityOutcome::Value(FlowValue::Record(
                    Arc::new(FlowRecord::from_iter([(
                        "doc".to_string(),
                        FlowValue::Projected(ProjectedValue::custom(
                            "doc",
                            self.descriptor.clone(),
                        )),
                    )])),
                ))),
                AbilityOp::Sleep(_) => {
                    self.parked.notify_one();
                    self.release.notified().await;
                    Ok(AbilityOutcome::Value(FlowValue::Null))
                }
                AbilityOp::Print(value) => {
                    let FlowValue::Projected(value) = value else {
                        panic!("the print must resolve the live exported descriptor")
                    };
                    self.rendered
                        .lock()
                        .expect("rendered text")
                        .push(value.render().expect("descriptor render"));
                    Ok(AbilityOutcome::Unit)
                }
                op => lashlang::testing::harness::EchoHost.perform(op).await,
            }
        }
    }
    let descriptor = Arc::new(SnapshotProjectedToolText::default());
    let parked = tokio::sync::Notify::new();
    let release = tokio::sync::Notify::new();
    let rendered = std::sync::Mutex::new(Vec::new());
    let host = DescriptorHost {
        descriptor: descriptor.clone(),
        parked: &parked,
        release: &release,
        rendered: &rendered,
    };
    let gate = lash_lashlang_runtime::HandOverGate::new();
    let service = lash_vm_client::service::Service::default();
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![park_tool_definition()]);
    let environment = LashlangSurface::default()
        .host_environment(&catalog)
        .expect("fixture surface");
    let run = lash_lashlang_runtime::WorkerRun {
        service: &service,
        host: &host,
        identities: lash_vm_broker::CodeCallIdentities::process_body(ProcessId::fixture(
            "descriptor-refusal",
        )),
        owner: lash_vm_protocol::VmOwner::new("descriptor-refusal"),
        frame_epoch: lash_vm_protocol::FrameEpoch(0),
        program: lash_vm_protocol::ProgramSource::Source {
            dialect: "typescript".into(),
            text: "const result = await cell.park({ value: 1 }); await sleep(60000); print(result.doc); finish(42);"
                .into(),
        },
        context: lash_vm_client::RunContext {
            environment,
            ..Default::default()
        },
        projected: Default::default(),
        bounds: lashlang::ExecutionBounds::new(
            lashlang::ExecutionBound::Unbounded,
            lashlang::ExecutionBound::Unbounded,
        ),
        state: lash_vm_protocol::StartState::Fresh,
        boundary: &|| false,
        hand_over: Some(&gate),
        projection_namespace: None,
    }
    .run();
    tokio::pin!(run);
    tokio::select! {
        () = parked.notified() => {}
        result = &mut run => panic!("descriptor cell ended before its parked wait: {result:?}"),
    }
    assert!(!gate.parked());
    assert_eq!(
        gate.refusal(),
        Some(lash_lashlang_runtime::HandOverRefusal::ExportedHostDescriptors { count: 1 })
    );
    assert_eq!(descriptor.materialize_count.load(Ordering::SeqCst), 0);
    release.notify_one();
    let lash_vm_broker::BrokeredEnd::Complete { value, .. } =
        run.await.expect("cell completes in place")
    else {
        panic!("the refused cell must complete in place")
    };
    assert_eq!(
        rmp_serde::from_slice::<ExecutionOutcome>(&value.0).expect("outcome"),
        ExecutionOutcome::Finished(FlowValue::Number(42.0))
    );
    assert_eq!(
        rendered.lock().expect("rendered text").as_slice(),
        &["rendered tool text"]
    );
    assert_eq!(descriptor.render_count.load(Ordering::SeqCst), 1);
    assert_eq!(descriptor.materialize_count.load(Ordering::SeqCst), 0);
}

fn park_tool_definition() -> lash_core::ToolDefinition {
    use lash_lashlang_runtime::{ToolBinding, ToolDefinitionBindingExt};

    lash_core::ToolDefinition::raw(
        "tool:cell_park",
        "cell_park",
        "Test-only effect used to park a cell continuation.",
        serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "number" } },
            "required": ["value"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "number" }),
    )
    .expect("valid declared tool schemas")
    .with_tool_binding(ToolBinding::new(["cell"], "park"))
}
