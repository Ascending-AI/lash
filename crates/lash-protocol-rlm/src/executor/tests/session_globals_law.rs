//! Law L11 (FIG-3571): session globals keep their names and cross-cell
//! visibility across cells and snapshot reload, whatever carrier a cell's
//! program travels in.
//!
//! Covers reassignment, block shadowing, root rebinding, a closure reading a
//! global, a deferred tool binding's result, a loop-carried global, and a cold
//! snapshot reload between cells.

use super::*;

fn context() -> lash_core::RuntimeExecutionContext<'static> {
    lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
        Arc::new(BindingRecordingDeferredProvider {
            executions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            observed_bindings: Arc::new(std::sync::Mutex::new(Vec::new())),
            enumerations: Default::default(),
        }),
        lash_core::ToolCatalog::default(),
        lash_core::testing::exec_code_invocation(
            "fig3571-l11",
            "turn-1",
            1,
            1,
            "exec-l11",
            "exec:l11",
        ),
    )
}

async fn run(state: &mut RlmExecutionState, code: &str) -> lash_core::ExecResponse {
    execute_code_with_channel_and_bounds(
        state,
        context(),
        ExecRequest {
            language: "typescript".to_string(),
            code: code.to_string(),
        },
        lashlang::global_in_memory_lashlang_artifact_store(),
        LashlangSurface::default(),
        Some(Arc::new(BindingDeferredResolver {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })),
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await
}

fn assert_authored_globals(state: &RlmExecutionState, names: &[&str], after: &str) {
    let live = state
        .rlm
        .globals()
        .iter()
        .map(|(name, _)| name.to_string())
        .collect::<BTreeSet<_>>();
    for name in names {
        assert!(
            live.contains(*name),
            "{after}: session global `{name}` keeps its authored name: {live:?}"
        );
    }
}

fn cold_reload(state: &RlmExecutionState) -> RlmExecutionState {
    let hydrated = state
        .hydrated_execution_state()
        .expect("the live execution state captures");
    let mut restored = RlmExecutionState::for_engine("typescript");
    restored
        .restore_execution_state(&hydrated)
        .expect("the captured execution state restores");
    restored
}

#[test]
fn session_globals_survive_cells_and_snapshot_reload() {
    block_on(async {
        let mut state = RlmExecutionState::for_engine("typescript");
        let first = run(
            &mut state,
            r#"let answer = 41;
const box = { n: 1 };
let counter = 0;
const fetched = await web.fetch({ url: "global" });
for (const item of [1, 2]) {
  counter = counter + item;
}
answer = answer + 1;"#,
        )
        .await;
        assert_eq!(first.error, None, "cell 1");
        assert_authored_globals(&state, &["answer", "box", "counter", "fetched"], "cell 1");

        let mut state = cold_reload(&state);
        let second = run(
            &mut state,
            r#"if (counter > 0) {
  let answer = 5;
  box.n = answer;
}
const add = (x) => x + answer;
const later = add(1);"#,
        )
        .await;
        assert_eq!(second.error, None, "cell 2");
        assert_authored_globals(
            &state,
            &["answer", "box", "counter", "fetched", "later"],
            "cell 2",
        );

        let mut state = cold_reload(&state);
        let third = run(
            &mut state,
            r#"const fetched = "rebound";
finish({ answer: answer, counter: counter, n: box.n, later: later, fetched: fetched });"#,
        )
        .await;
        assert_eq!(third.error, None, "cell 3");
        let rendered = third.terminal_finish.expect("cell 3 finishes").to_string();
        for expected in [
            "\"answer\":42",
            "\"counter\":3",
            "\"n\":5",
            "\"later\":43",
            "\"fetched\":\"rebound\"",
        ] {
            assert!(
                rendered.contains(expected),
                "cell 3 reads every global by name after two reloads: missing {expected} in {rendered}"
            );
        }
        assert_authored_globals(&state, &["answer", "fetched", "later"], "cell 3");
    });
}
