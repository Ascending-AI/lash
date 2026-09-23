//! Law L11 (FIG-3571): session globals keep their names and cross-cell
//! visibility across cells and snapshot reload, whatever carrier a cell's
//! program travels in, and a front end's private slots never become one.
//!
//! Covers reassignment, block shadowing (including a later cell whose shadow
//! lowers to the same generated slot), root rebinding, member assignment, a
//! closure over a shadowed block binding called after its block ends (closures
//! never cross a cell, so none is called after a reload), a process
//! handle, a projected host binding, a deferred tool binding's result, a
//! loop-carried global, and a cold snapshot reload between cells. After every
//! cell and every reload the session's globals are exactly the authored ones.

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
    run_in(state, context(), code).await
}

async fn run_in(
    state: &mut RlmExecutionState,
    context: lash_core::RuntimeExecutionContext<'static>,
    code: &str,
) -> lash_core::ExecResponse {
    execute_code_with_channel_and_bounds(
        state,
        context,
        ExecRequest {
            language: "typescript".to_string(),
            code: code.to_string(),
        },
        lashlang::global_in_memory_lashlang_artifact_store(),
        LashlangSurface::default(),
        Some(Arc::new(BindingDeferredResolver {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })),
        RlmProjectedBindings::new()
            .bind_json("host_config", serde_json::json!({ "label": "from-host" }))
            .expect("the projected binding is unique"),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig::default(),
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await
}

/// The session's globals are exactly the authored bindings: every authored
/// name survives under its own name, and nothing else does — no private slot
/// a front end generated, and no projected host binding.
fn assert_exact_globals(state: &RlmExecutionState, names: &[&str], after: &str) {
    let live = state
        .rlm
        .globals()
        .iter()
        .map(|(name, _)| name.to_string())
        .collect::<BTreeSet<_>>();
    let expected = names
        .iter()
        .map(|name| (*name).to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        live, expected,
        "{after}: the session's globals are exactly the authored bindings"
    );
}

/// A cell context whose tool surface can start a process.
async fn process_context() -> lash_core::RuntimeExecutionContext<'static> {
    let artifact_store: Arc<dyn lashlang::LashlangArtifactStore> =
        lashlang::global_in_memory_lashlang_artifact_store();
    let process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore> =
        Arc::new(lash_core::facade_support::InMemoryProcessExecutionEnvStore::new());
    let effect_host = memory_effect_host().await;
    let session_policy = lash_core::SessionPolicy {
        model: lash_core::ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("L11 test model"),
        ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
    };
    let processes: Arc<dyn lash_core::ProcessService> = Arc::new(TypeScriptSignalProcessService {
        registry: Arc::new(lash_core::TestLocalProcessRegistry::default()),
        effect_host: Arc::clone(&effect_host),
        originator_override: None,
        env_store: Arc::clone(&process_env_store),
        engines: fixture_process_engines(artifact_store, LashlangSurface::default()),
    });
    lash_core::testing::code_execution_context_with_process_dependencies(
        Arc::new(ProcessControlToolProvider),
        process_control_tool_catalog(),
        None,
        processes,
        effect_host,
        process_env_store,
        lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            session_policy,
        ),
    )
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

/// ECMA-262 block scoping (ADR 0062/0064): a `let`/`const` declared inside a
/// block, a loop binder included, does not exist after its block, so the
/// front end marks it private like its own slots. `item` (the loop binder)
/// and `armed` (a `const` in an `if` arm) are therefore not session globals;
/// the top-level `let answer` and `let counter` are.
const CELL_1_GLOBALS: &[&str] = &["answer", "box", "counter", "fetched", "total"];
/// `reader` is absent by design: a closure never crosses a program boundary
/// (`lashlang::State::install_runtime` drops closure-rooted globals), so the
/// closure over a shadowed block binding is exercised inside its own cell.
const CELL_2_GLOBALS: &[&str] = &[
    "answer",
    "box",
    "counter",
    "fetched",
    "from_host",
    "total",
    "worker",
    "handle",
    "later",
];

#[test]
fn session_globals_survive_cells_and_reload_and_private_slots_never_do() {
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
if (counter > 0) {
  const armed = counter;
  box.n = armed;
}
{
  const answer = 100;
  box.n = answer;
}
box.n += 1;
answer = answer + 1;
const total = [1, 2, 3].map((value) => value * 2).length;"#,
        )
        .await;
        assert_eq!(first.error, None, "cell 1");
        assert_exact_globals(&state, CELL_1_GLOBALS, "cell 1");
        let mut state = cold_reload(&state);
        assert_exact_globals(&state, CELL_1_GLOBALS, "reload after cell 1");

        let second = run_in(
            &mut state,
            process_context().await,
            r#"let reader = null;
if (counter > 0) {
  let answer = 5;
  reader = () => answer;
}
const worker = async () => await waitSignal("ready");
const handle = await processes.start({ definition: worker });
const later = reader() + answer;
const from_host = host_config.label;"#,
        )
        .await;
        assert_eq!(second.error, None, "cell 2");
        assert_exact_globals(&state, CELL_2_GLOBALS, "cell 2");
        let mut state = cold_reload(&state);
        assert_exact_globals(&state, CELL_2_GLOBALS, "reload after cell 2");

        // Cell 3's block shadow lowers to the same generated slot cell 2's
        // did; neither survives its cell, so nothing stale collides.
        let third = run(
            &mut state,
            r#"if (counter > 0) {
  let answer = 7;
  box.n = answer;
}
const fetched = "rebound";
finish({ answer: answer, counter: counter, n: box.n, later: later, fetched: fetched, from_host: from_host, total: total, handle: typeof handle });"#,
        )
        .await;
        assert_eq!(third.error, None, "cell 3");
        assert_exact_globals(&state, CELL_2_GLOBALS, "cell 3");
        let rendered = third.terminal_finish.expect("cell 3 finishes").to_string();
        for expected in [
            "\"answer\":42",
            "\"counter\":3",
            "\"n\":7",
            "\"later\":47",
            "\"fetched\":\"rebound\"",
            "\"from_host\":\"from-host\"",
            "\"total\":3",
            "\"handle\":\"object\"",
        ] {
            assert!(
                rendered.contains(expected),
                "cell 3 reads every global by name after two reloads: missing {expected} in {rendered}"
            );
        }
        let state = cold_reload(&state);
        assert_exact_globals(&state, CELL_2_GLOBALS, "reload after cell 3");
    });
}
