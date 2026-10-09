//! The laws of saved functions (FIG-5772): a function a cell binds at its
//! top level is saved as a self-contained value when the cell is accepted,
//! and a later cell calls it by name.

use std::sync::Arc;

use super::{
    CellTools, SESSION, TURN, finish_of, open_host, typescript_services, typescript_state,
};
use crate::testing::{cell_context, run_cell};

/// A function survives across cells: defined in cell 1, called in cell 3.
#[tokio::test(flavor = "multi_thread")]
async fn a_function_a_cell_defines_is_called_two_cells_later() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let services = typescript_services(None);
    let mut state = typescript_state();

    let defined = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        &services,
        "const step = 2;\nfunction advance(value) { return value + step; }\nconst twice = (value) => advance(advance(value));",
    )
    .await;
    assert!(defined.error().is_none(), "{:?}", defined.error());

    let between = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:1", tools.clone()),
        &services,
        "const base = 38;",
    )
    .await;
    assert!(between.error().is_none(), "{:?}", between.error());

    let called = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:2", tools),
        &services,
        "finish(twice(base));",
    )
    .await;
    assert_eq!(
        finish_of(&called),
        serde_json::json!(42),
        "cell 3 calls the function cell 1 defined, and the function it calls by name"
    );
}

/// One cell of `state`, which must complete.
async fn cell(
    state: &mut super::RlmExecutionState,
    host: &crate::testing::DurableHost,
    key: &'static str,
    tools: Arc<dyn lash_core::ToolProvider>,
    code: &str,
) -> lash_core::ExecResponse {
    let response = run_cell(
        state,
        cell_context(host, SESSION, TURN, key, tools),
        &typescript_services(None),
        code,
    )
    .await;
    assert!(response.error().is_none(), "{:?}", response.error());
    response
}

/// A provider that offers no tool at all.
pub(super) struct NoTools;

#[async_trait::async_trait]
impl lash_core::ToolProvider for NoTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        Vec::new()
    }

    fn resolve_manifest_by_id(&self, _id: &lash_core::ToolId) -> Option<lash_core::ToolManifest> {
        None
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<lash_core::ToolContract>> {
        None
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        unreachable!("no tool is offered")
    }
}

/// A saved function survives a restart: the session's state is captured,
/// loaded cold into another node's state, and the function is called
/// there.
#[tokio::test(flavor = "multi_thread")]
async fn a_saved_function_is_called_after_a_cold_restore() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut state = typescript_state();
    cell(
        &mut state,
        &host,
        "exec-code:0",
        tools.clone(),
        "const rate = 3;\nfunction scale(value: number): number { return value * rate; }",
    )
    .await;

    let fleet = lash_core::FleetFormat::current();
    state
        .snapshot_execution_state(fleet)
        .await
        .expect("capture the session's state");
    state.acknowledge_execution_state_capture();
    let saved = state
        .hydrated_execution_state(fleet)
        .await
        .expect("the saved session state");
    drop(state);
    let mut restored = typescript_state();
    restored
        .restore_execution_state(&saved, fleet)
        .await
        .expect("load the session's state");

    let called = cell(
        &mut restored,
        &host,
        "exec-code:1",
        tools,
        "finish(scale(14));",
    )
    .await;
    assert_eq!(finish_of(&called), serde_json::json!(42));
}

/// A saved function seeds a new session: one created with it as an input
/// calls it, and holds nothing else of the session it came from.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_session_created_with_a_saved_function_calls_it() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut source = typescript_state();
    cell(
        &mut source,
        &host,
        "exec-code:0",
        tools.clone(),
        "const greeting = \"hello\";\nconst greet = (name) => `${greeting}, ${name}`;",
    )
    .await;
    let mut functions = source.saved_functions();
    functions.retain(|name, _| name == "greet");
    drop(source);

    let mut seeded = typescript_state();
    seeded
        .seed_functions(&functions, &std::collections::BTreeSet::new())
        .await
        .expect("create the session with the function");
    assert_eq!(
        seeded.binding_names().collect::<Vec<_>>(),
        Vec::<&str>::new(),
        "the new session holds the function and no binding of the old one"
    );
    let called = cell(
        &mut seeded,
        &host,
        "exec-code:1",
        tools,
        "finish(greet(\"lash\"));",
    )
    .await;
    assert_eq!(finish_of(&called), serde_json::json!("hello, lash"));
}

/// A saved function is refused where a tool it needs is missing: a session
/// whose environment lacks an effect it calls gets the typed refusal,
/// naming the function and the effect, before anything runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_saved_function_is_refused_where_a_tool_it_calls_is_missing() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut source = typescript_state();
    cell(
        &mut source,
        &host,
        "exec-code:0",
        tools.clone(),
        "async function shout(text) { return await echo.say({ text }); }\nconst once = await shout(\"a\");",
    )
    .await;
    assert_eq!(tools.answered(), vec!["a"]);
    assert!(
        source.saved_functions().contains_key("shout"),
        "{:?}",
        source.bindings().not_carried()
    );

    let mut seeded = typescript_state();
    seeded
        .seed_functions(
            &source.saved_functions(),
            &std::collections::BTreeSet::new(),
        )
        .await
        .expect("create the session with the function");
    let refused = run_cell(
        &mut seeded,
        cell_context(&host, SESSION, TURN, "exec-code:1", Arc::new(NoTools)),
        &typescript_services(None),
        "finish(await shout(\"b\"));",
    )
    .await;
    let failure = refused.error().expect("the cell is refused");
    assert!(
        failure.message.contains("TS_SAVED_FUNCTION_UNUSABLE")
            && failure.message.contains(
                "the saved function `shout` calls `echo.say`, which this session does not offer"
            ),
        "{}",
        failure.message
    );
    assert_eq!(tools.answered(), vec!["a"], "nothing of the cell ran");
}

/// Captures are frozen: a later cell changes a top-level variable the
/// function read, and the function still sees the value its cell left.
#[tokio::test(flavor = "multi_thread")]
async fn a_saved_function_keeps_the_captures_its_cell_left() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut state = typescript_state();
    cell(
        &mut state,
        &host,
        "exec-code:0",
        tools.clone(),
        "let factor = 2;\nconst limits = { top: 10 };\nconst clamp = (value) => Math.min(value * factor, limits.top);",
    )
    .await;
    cell(
        &mut state,
        &host,
        "exec-code:1",
        tools.clone(),
        "factor = 100;\nlimits.top = 1000;",
    )
    .await;
    let called = cell(
        &mut state,
        &host,
        "exec-code:2",
        tools,
        "finish({ small: clamp(3), large: clamp(50), factor, top: limits.top });",
    )
    .await;
    assert_eq!(
        finish_of(&called),
        serde_json::json!({ "small": 6, "large": 10, "factor": 100, "top": 1000 }),
        "the function reads the factor and the limits as cell 1 left them"
    );
}

/// A function that captures a task is not carried: its cell reports the
/// binding as not carried, the session records the cause by name, and a
/// later cell does not have the binding.
#[tokio::test(flavor = "multi_thread")]
async fn a_function_that_captures_a_task_is_not_carried_and_says_so() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut state = typescript_state();
    let defined = cell(
        &mut state,
        &host,
        "exec-code:0",
        tools.clone(),
        "const work = async () => 1;\nconst pending = work();\nawait pending;\nconst later = () => pending;",
    )
    .await;
    assert_eq!(
        *defined.bindings,
        lash_core::BindingChanges {
            added: vec!["work".to_owned()],
            not_carried: vec!["later".to_owned(), "pending".to_owned()],
            ..Default::default()
        }
    );
    let why = state
        .bindings()
        .not_carried()
        .get(&lash_kernel_doc::Name::new("later"))
        .expect("the session records why `later` was not kept");
    assert_eq!(
        why,
        &lash_kernel_dialect::NotSaved::Capture {
            name: lash_kernel_doc::Name::new("pending"),
            why: lash_kernel_dialect::CaptureRefusal::Task,
        }
    );
    assert_eq!(
        why.to_string(),
        "its function reads `pending`, which holds a task or a pending promise"
    );

    let refused = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:1", tools),
        &typescript_services(None),
        "finish(later());",
    )
    .await;
    assert!(
        refused.error().is_some(),
        "`later` is not bound in a later cell"
    );
}
