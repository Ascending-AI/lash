use super::*;

#[test]
fn return_runs_finally_while_finish_stops_the_cell() {
    assert_eq!(
        finished(
            "const trace = []; function f() { try { return 'returned'; } finally { trace.push('finally'); } } const result = f(); finish({ result, trace });"
        ),
        lashlang::from_json(serde_json::json!({"result":"returned", "trace":["finally"]}))
    );
    assert_eq!(
        finished("const trace = []; try { finish(trace); } finally { trace.push('finally'); }"),
        lashlang::from_json(serde_json::json!([]))
    );
}

#[test]
fn pending_handle_survives_park_without_cross_cell_export() {
    let environment = lashlang::LashlangHostEnvironment::new(
        two_leaf_web_environment().resources,
        lashlang::LashlangAbilities::all(),
    );
    let linked = lash_typescript::link("const p = web.fetch({ value: 'kept' }); const nested = [p]; await sleep(5); finish(await p);", &environment).expect("cell links");
    let compiled = lashlang::testing::harness::compile_linked_main(&linked);
    futures::executor::block_on(async {
        let mut state = State::new();
        let host = ProcessDurabilityHost;
        let execution = lashlang::ExecutionEnvironment::new(&host).process();
        let mut vm = Vm::from_state(&compiled, &mut state, &execution).expect("VM");
        assert_eq!(
            vm.run_process_until_effect()
                .await
                .expect("sleep completes"),
            VmRunOutcome::EffectCompleted
        );
        let continuation = vm.suspend().expect("park");
        assert_eq!(
            continuation.pending_tools.values().flatten().count(),
            1,
            "park carries the unconsumed request, not merely the handle record"
        );
        let bytes = serde_json::to_vec(&continuation).expect("encode");
        let mut resumed = Vm::resume_from(
            serde_json::from_slice(&bytes).expect("decode"),
            &compiled,
            &execution,
        )
        .expect("resume");
        loop {
            match resumed.run_process_until_effect().await.expect("run") {
                VmRunOutcome::EffectCompleted => {}
                VmRunOutcome::Complete(outcome) => {
                    assert_eq!(
                        outcome,
                        ExecutionOutcome::Finished(Value::String("kept".into()))
                    );
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        let globals = resumed.into_globals().expect("export state");
        state = State::from_snapshot(lashlang::Snapshot::new(globals));
        assert!(state.globals().get("p").is_none());
        assert!(
            state.globals().get("nested").is_none(),
            "an aggregate reaching a handle is not a session global"
        );
        let snapshot = state.snapshot().to_canonical_bytes().expect("snapshot");
        let mut next_state = State::from_snapshot(
            lashlang::Snapshot::from_canonical_bytes(&snapshot).expect("restore session"),
        );
        let next = lash_typescript::link(
            "const q = web.fetch({ value: 'new' }); finish(await q);",
            &environment,
        )
        .expect("next cell");
        assert_eq!(
            lashlang::execute(
                &lashlang::testing::harness::compile_linked_main(&next),
                &mut next_state,
                &host
            )
            .await
            .expect("new request"),
            ExecutionOutcome::Finished(Value::String("new".into()))
        );
    });
}

#[test]
pub(super) fn literal_elisions_are_holes_for_in_has_own_property_and_iteration() {
    let fixtures: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("literal_elisions.json")).expect("Node fixtures");
    for fixture in fixtures {
        let literal = fixture["literal"].as_str().expect("literal");
        let probe = fixture["probe"].as_str().expect("probe");
        let expected = lashlang::from_json(fixture["expected"].clone());
        assert!(
            include_str!("../../README.md").contains(literal),
            "the admitted literal has a public teaching example: {literal}"
        );
        let registered =
            format!("(function () {{ const a = {literal}; return JSON.stringify({probe}); }})()");
        assert!(
            include_str!("../differential/findings/FIG-4151.txt")
                .lines()
                .any(|row| row == registered),
            "the admitted literal has a registered Node corpus witness: {literal}"
        );
        assert_eq!(
            finished(&format!("const a = {literal}; finish({probe});")),
            expected,
            "{literal}: straight through"
        );
        let mut state = State::new();
        let seed = lash_typescript::testing::compile(&format!("const a = {literal};"))
            .expect("sparse seed");
        futures::executor::block_on(lashlang::execute(&seed, &mut state, &Host))
            .expect("seed executes");
        let snapshot = state.snapshot();
        let bytes = snapshot
            .to_canonical_bytes()
            .expect("canonical sparse snapshot");
        {
            let restored =
                lashlang::Snapshot::from_canonical_bytes(&bytes).expect("canonical restore");
            let mut state = State::from_snapshot(restored);
            let linked = lash_typescript::link(
                &format!("finish({probe});"),
                &lashlang::LashlangHostEnvironment::new(
                    lashlang::LashlangHostCatalog::new(),
                    lashlang::LashlangAbilities::all(),
                )
                .with_globals(["a"]),
            )
            .expect("restored probe links");
            let outcome = futures::executor::block_on(lashlang::execute(
                &lashlang::testing::harness::compile_linked_main(&linked),
                &mut state,
                &Host,
            ))
            .expect("snapshot probe executes");
            assert_eq!(
                outcome,
                ExecutionOutcome::Finished(expected.clone()),
                "{literal}: session snapshot retains holes"
            );
        }
        let source = format!(
            "const worker = async () => {{ const a = {literal}; await sleep(5); return {probe}; }}; finish(await processes.start({{ definition: worker }}));"
        );
        assert_eq!(
            suspend_and_resume_process(&source, serde_json::json!({}), 0),
            ExecutionOutcome::Finished(expected),
            "{literal}: restored holes and iteration"
        );
    }
}
