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
    let services = typescript_services();
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
        "await control.finish(twice(base));",
    )
    .await;
    assert_eq!(
        finish_of(&called),
        serde_json::json!(42),
        "cell 3 calls the function cell 1 defined, and the function it calls by name"
    );
}

/// S8 carry of a cell-defined function (FIG-6193): `bump`, defined in
/// cell 1 over a top-level object, is a binding later cells call. Each
/// call installs a fresh copy of what it froze, so two calls in cell 2 are
/// independent and neither reaches the session's own `box`; and cell 3,
/// parked at a held call, its node killed, resumed on another node and a
/// fresh worker from a session restored cold, calls it the same way.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_defined_function_is_called_after_a_park_and_reopen_with_fresh_captures() {
    use std::sync::atomic::Ordering;

    let mut host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut state = typescript_state();
    cell(
        &mut state,
        &host,
        "exec-code:0",
        tools.clone(),
        "const box = { n: 0 };\nfunction bump() { box.n += 1; return box.n; }",
    )
    .await;
    let twice = cell(
        &mut state,
        &host,
        "exec-code:1",
        tools.clone(),
        "await control.finish([bump(), bump(), box.n]);",
    )
    .await;
    assert_eq!(
        finish_of(&twice),
        serde_json::json!([1, 1, 0]),
        "each call has captures of its own, and none is the session's"
    );
    crate::testing::commit_working(&mut state, &host).await;
    let saved = state
        .hydrated_execution_graph()
        .await
        .expect("the working descriptor");
    let code = "let said = \"unset\";\ntry { said = await echo.say({ text: \"held\" }); } catch (error) { said = \"interrupted\"; }\nawait control.finish([bump(), bump(), said]);";
    tools.hold.store(true, Ordering::SeqCst);
    let services = typescript_services();
    {
        let parked = run_cell(
            &mut state,
            cell_context(&host, SESSION, TURN, "exec-code:2", tools.clone()),
            &services,
            code,
        );
        tokio::select! {
            response = parked => panic!("the cell ended with its call held: {:?}", response.result),
            () = tools.held_started.notified() => {}
        }
    }
    drop(state);
    host.kill_and_resume().await;
    tools.hold.store(false, Ordering::SeqCst);
    let mut reopened = typescript_state();
    reopened
        .restore_execution_graph(&saved, lash_core::plugin::ExecutionGraphPurpose::Working)
        .expect("the working state restores");
    let resumed = run_cell(
        &mut reopened,
        cell_context(&host, SESSION, TURN, "exec-code:2", tools),
        &services,
        code,
    )
    .await;
    assert_eq!(
        finish_of(&resumed),
        serde_json::json!([1, 1, "interrupted"]),
        "the reopened cell calls the function cell 1 defined, with fresh captures"
    );
}

/// A saved function is the value its cell left, name and length included:
/// a later cell reads them, a deletion of one lasts into the next cell, a
/// binding that takes the function keeps its name, and a function made
/// inside another and returned still calls the one it closes over.
#[tokio::test(flavor = "multi_thread")]
async fn a_saved_function_keeps_its_name_and_length_across_cells() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut state = typescript_state();
    cell(
        &mut state,
        &host,
        "exec-code:0",
        tools.clone(),
        "function advance(a, b = 1) { return a + b; }\nconst make = () => { function inner(x) { return x + 1; } return (n) => inner(n); };\nconst bump = make();",
    )
    .await;
    cell(
        &mut state,
        &host,
        "exec-code:1",
        tools.clone(),
        "const alias = advance;\ndelete (advance as any).length;",
    )
    .await;
    let called = cell(
        &mut state,
        &host,
        "exec-code:2",
        tools,
        "await control.finish({ name: advance.name, length: advance.length, alias: alias.name, bump: bump.length, bumped: bump(1), called: alias(2) });",
    )
    .await;
    assert_eq!(
        finish_of(&called),
        serde_json::json!({ "name": "advance", "length": 0, "alias": "advance", "bump": 1, "bumped": 2, "called": 3 }),
        "the functions keep the name, length and deletion their cells left"
    );
}

/// Two bindings of one function are one function in every later cell
/// (FIG-5826), on this node and after a cold restore: aliases made in the
/// function's own cell and in a later one stay `===`, and a property
/// deleted through one is gone through all.
#[tokio::test(flavor = "multi_thread")]
async fn aliases_of_a_saved_function_stay_one_function_across_cells() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut state = typescript_state();
    cell(
        &mut state,
        &host,
        "exec-code:0",
        tools.clone(),
        "function f(a: number) { return a; }\nconst g = f;",
    )
    .await;
    cell(
        &mut state,
        &host,
        "exec-code:1",
        tools.clone(),
        "const h = f;\ndelete (g as any).length;",
    )
    .await;
    crate::testing::commit_working(&mut state, &host).await;
    let saved = state
        .hydrated_execution_graph()
        .await
        .expect("the saved session state");
    drop(state);
    let mut state = typescript_state();
    state
        .restore_execution_graph(&saved, lash_core::plugin::ExecutionGraphPurpose::Working)
        .expect("load the session's state");
    let read = cell(
        &mut state,
        &host,
        "exec-code:2",
        tools,
        "await control.finish({ fg: f === g, fh: f === h, f: f.length, g: g.length, h: h.length });",
    )
    .await;
    assert_eq!(
        finish_of(&read),
        serde_json::json!({ "fg": true, "fh": true, "f": 0, "g": 0, "h": 0 }),
        "every alias is the one function, deletion included"
    );
}

/// A saved function is a `Map` key and a `Set` member in a later cell
/// (FIG-5826), under any of its aliases, and a key read back is the
/// function.
#[tokio::test(flavor = "multi_thread")]
async fn a_saved_function_is_a_map_key_and_set_member_in_a_later_cell() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut state = typescript_state();
    cell(
        &mut state,
        &host,
        "exec-code:0",
        tools.clone(),
        "function f() { return 1; }\nconst g = f;",
    )
    .await;
    let keyed = cell(
        &mut state,
        &host,
        "exec-code:1",
        tools,
        "const m = new Map([[f, \"x\"]]);\nconst s = new Set([f]);\nconst key = [...m.keys()][0];\nawait control.finish({ get: m.get(g), has: s.has(g), size: m.size, called: key(), same: key === f });",
    )
    .await;
    assert_eq!(
        finish_of(&keyed),
        serde_json::json!({ "get": "x", "has": true, "size": 1, "called": 1, "same": true }),
        "the function keys the map and the set, and comes back out as itself"
    );
}

/// One cell of `state`, which must complete.
async fn cell(
    state: &mut super::CodeModeExecutionState,
    host: &crate::testing::DurableHost,
    key: &'static str,
    tools: Arc<dyn lash_core::ToolProvider>,
    code: &str,
) -> lash_core::ExecResponse {
    let response = run_cell(
        state,
        cell_context(host, SESSION, TURN, key, tools),
        &typescript_services(),
        code,
    )
    .await;
    assert!(response.error().is_none(), "{:?}", response.error());
    response
}

/// A fresh session restored from `source`'s state, which `host` commits
/// as its turn's working state first.
async fn restored(
    host: &crate::testing::DurableHost,
    source: &mut super::CodeModeExecutionState,
) -> super::CodeModeExecutionState {
    crate::testing::commit_working(source, host).await;
    let saved = source
        .hydrated_execution_graph()
        .await
        .expect("the saved session state");
    let mut restored = typescript_state();
    restored
        .restore_execution_graph(&saved, lash_core::plugin::ExecutionGraphPurpose::Working)
        .expect("load the session's state");
    restored
}

/// A provider that offers no tool at all.
pub(super) struct NoTools;

#[async_trait::async_trait]
impl lash_core::ToolProvider for NoTools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(Vec::new())
    }

    fn resolve_manifest_by_id(
        &self,
        _id: &lash_vocabulary::tool_contract::ToolId,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<lash_vocabulary::tool_contract::ToolManifest> {
        None
    }

    fn resolve_contract(
        &self,
        _name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        None
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        unreachable!("no tool is offered")
    }
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
        source
            .lowering_functions()
            .contains_key(&lash_kernel_value::names::Name::new("shout")),
        "{:?}",
        source.not_carried()
    );

    let mut seeded = restored(&host, &mut source).await;
    let ctx = cell_context(&host, SESSION, TURN, "exec-code:1", Arc::new(NoTools));
    let refused = run_cell(
        &mut seeded,
        ctx.clone(),
        &typescript_services(),
        "await control.finish(await shout(\"b\"));",
    )
    .await;
    assert!(refused.error().is_some(), "the cell is refused");
    assert_package_refusal(&ctx, false);
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
        "await control.finish({ small: clamp(3), large: clamp(50), factor, top: limits.top });",
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
    let not_carried = state.not_carried();
    let why = not_carried
        .get(&lash_kernel_value::names::Name::new("later"))
        .expect("the session records why `later` was not kept");
    assert_eq!(
        why,
        &lash_kernel_dialect::NotSaved::Capture {
            name: lash_kernel_value::names::Name::new("pending"),
            why: lash_kernel_dialect::CaptureRefusal::Task,
        }
        .to_string()
    );
    assert_eq!(
        why,
        "its function reads `pending`, which holds a task or a pending promise"
    );

    let refused = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:1", tools),
        &typescript_services(),
        "await control.finish(later());",
    )
    .await;
    assert!(
        refused.error().is_some(),
        "`later` is not bound in a later cell"
    );
}

/// Offers `echo.say` as a tool that ends the turn: what a host declares
/// after a function calling the ordinary `echo.say` was saved.
struct EchoEndsTheTurn;

impl EchoEndsTheTurn {
    fn definition() -> lash_vocabulary::tool_contract::ToolDefinition {
        lash_vocabulary::tool_contract::ToolDefinition::control(
            "tool:echo",
            "echo.say",
            "Echo the text back, ending the turn",
            lash_vocabulary::tool_contract::ToolDefinition::default_input_schema(),
            lash_kernel_value::tool_declaration::TurnControls::finish(
                lash_kernel_value::json_schema::JsonSchema::any(),
            ),
        )
        .expect("valid declared tool schema")
        .with_execution(std::time::Duration::from_secs(120))
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for EchoEndsTheTurn {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(vec![Self::definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        (name == "echo.say" || name == "tool:echo").then(|| Arc::new(Self::definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::finish(call.args.clone()).into()
    }
}

/// Law 12 (FIG-5781): a saved function pins what its effects declared. A
/// session where an effect it calls now ends the turn refuses the call of
/// the function, with the typed refusal, before anything runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_saved_function_is_refused_where_a_tool_it_calls_now_ends_the_turn() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut source = typescript_state();
    cell(
        &mut source,
        &host,
        "exec-code:0",
        tools.clone(),
        "async function shout(text) { return await echo.say({ text }); }",
    )
    .await;
    let mut seeded = restored(&host, &mut source).await;
    let ctx = cell_context(
        &host,
        SESSION,
        TURN,
        "exec-code:1",
        Arc::new(EchoEndsTheTurn),
    );
    let refused = run_cell(
        &mut seeded,
        ctx.clone(),
        &typescript_services(),
        "const said = await shout(\"b\");",
    )
    .await;
    assert!(refused.error().is_some(), "the cell is refused");
    assert_package_refusal(&ctx, true);
    assert!(refused.tool_calls.is_empty(), "nothing of the cell ran");
}

/// Law 12 (FIG-5781): a function cannot end the turn, so one that calls
/// `control.finish` is refused before its cell runs, and the session keeps
/// no such function.
#[tokio::test(flavor = "multi_thread")]
async fn a_function_that_calls_control_finish_is_refused_and_never_saved() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut state = typescript_state();
    let refused = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        &typescript_services(),
        "async function done(value) { await control.finish(value); }\nawait echo.say({ text: \"ran\" });",
    )
    .await;
    let failure = refused.error().expect("the cell is refused");
    assert!(
        failure.message.contains("TS_CONTROL_CALL_PLACEMENT"),
        "{}",
        failure.message
    );
    assert!(tools.answered().is_empty(), "nothing of the cell ran");
    assert!(
        !state
            .lowering_functions()
            .contains_key(&lash_kernel_value::names::Name::new("done"))
    );
}

/// TS-CALL-SLOTS: a later cell enters the packed wrapper of the shared
/// positional body, preserving unknown types, spreads and apply.
#[tokio::test(flavor = "multi_thread")]
async fn a_positional_function_saved_in_one_cell_accepts_generic_calls_in_the_next() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let services = typescript_services();
    let mut state = typescript_state();
    let defined = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        &services,
        "function f(value = 7) { return value; }",
    )
    .await;
    assert!(defined.error().is_none(), "{:?}", defined.error());
    let called = run_cell(&mut state,
        cell_context(&host, SESSION, TURN, "exec-code:1", tools), &services,
        "await control.finish({ plain: f('x'), spread: f(...['y']), apply: f.apply(undefined, ['z']), missing: f(), length: f.length });").await;
    assert_eq!(
        finish_of(&called),
        serde_json::json!({ "plain": "x", "spread": "y", "apply": "z", "missing": 7, "length": 0 })
    );
}

/// Relink refusals retain the package cause across the host boundary.
pub(super) fn assert_package_refusal(ctx: &lash_core::RuntimeExecutionContext<'_>, changed: bool) {
    use lash_execution_contracts::process::linking::{AdmissionRefusal, EnginePortError};
    let error = ctx
        .take_nested_effect_error()
        .expect("typed relink failure")
        .into_runtime_error();
    let Some(lash_core::RuntimeErrorCause::PluginOperation { failure }) = error.cause else {
        panic!("relink lost its typed package cause: {error:?}");
    };
    let lash_execution_contracts::plugin_error::PluginError::Package(error) =
        serde_json::from_value(failure.payload).expect("decode package cause")
    else {
        panic!("relink changed its package cause");
    };
    let name = match (*error, changed) {
        (EnginePortError::Refused(AdmissionRefusal::MissingImport { name, .. }), false)
        | (EnginePortError::Refused(AdmissionRefusal::SignatureMismatch { name, .. }), true) => {
            name
        }
        (other, _) => panic!("unexpected relink cause: {other:?}"),
    };
    assert_eq!(name.to_string(), "echo.say");
}

/// FIG-7180 / ADR 0113: a saved function's declaration belongs to its frame.
/// Ending the executions that used it must neither discard nor republish it;
/// ending the frame releases its final artifact edge.
#[tokio::test(flavor = "multi_thread")]
async fn saved_function_declarations_live_until_their_frame_ends() {
    use lash_core_store::artifact_referrer::ResolvedArtifactCleanup;
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let services = typescript_services();
    let mut state = typescript_state();
    let first = cell_context(&host, SESSION, TURN, "frame-declaration:0", tools.clone());
    let first_referrer = first.execution_referrer().unwrap();
    let defined = run_cell(
        &mut state,
        first,
        &services,
        "function keep(value) { return value; }",
    )
    .await;
    assert!(defined.error().is_none(), "{:?}", defined.error());
    let document = state.lowering_functions()[&lash_kernel_value::Name::new("keep")]
        .document
        .id()
        .to_string();
    let second = cell_context(&host, SESSION, TURN, "frame-declaration:1", tools);
    let second_referrer = second.execution_referrer().unwrap();
    let frame = second.frame_referrer().unwrap();
    let called = run_cell(
        &mut state,
        second,
        &services,
        "await control.finish(keep(7));",
    )
    .await;
    assert_eq!(finish_of(&called), serde_json::json!(7));
    let context = host.context();
    let store = context.backend().module_artifacts();
    for referrer in [first_referrer, second_referrer] {
        store
            .end_module_referrer(&ResolvedArtifactCleanup {
                referrer,
                carries: Vec::new(),
            })
            .await
            .unwrap();
    }
    assert!(
        store
            .get_module_artifact(&document)
            .await
            .unwrap()
            .is_some(),
        "a live frame lost its saved function declaration when an execution ended"
    );
    store
        .end_module_referrer(&ResolvedArtifactCleanup {
            referrer: frame,
            carries: Vec::new(),
        })
        .await
        .unwrap();
    assert!(
        store
            .get_module_artifact(&document)
            .await
            .unwrap()
            .is_none(),
        "the ended frame retained a saved declaration artifact"
    );
}

/// FIG-6926 / RELINK-CLOSURE: a contract-only change rebinds the saved
/// callable's heap origin too, including aliases and data-held references.
#[tokio::test(flavor = "multi_thread")]
async fn a_saved_function_and_its_aliases_relink_to_the_next_cells_operation_contract() {
    struct EchoContract(&'static str);
    impl EchoContract {
        fn definition(&self) -> lash_vocabulary::tool_contract::ToolDefinition {
            lash_vocabulary::tool_contract::ToolDefinition::raw(
                "tool:echo",
                "echo.say",
                "Echo the text back",
                lash_vocabulary::tool_contract::ToolDefinition::default_input_schema(),
                serde_json::json!({"type": "string"}),
            )
            .expect("echo schema")
            .declared_contract(lash_kernel_value::operations::ContractId::new(self.0))
            .with_execution(std::time::Duration::from_secs(120))
        }
    }
    #[async_trait::async_trait]
    impl lash_core::ToolProvider for EchoContract {
        fn tool_manifests(
            &self,
            _: &lash_core::OwnerConfigView,
        ) -> Result<
            Vec<lash_vocabulary::tool_contract::ToolManifest>,
            lash_execution_contracts::plugin_error::PluginError,
        > {
            Ok(vec![self.definition().manifest()])
        }
        fn resolve_contract(
            &self,
            name: &str,
            _: &lash_core::OwnerConfigView,
        ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
            (name == "echo.say" || name == "tool:echo")
                .then(|| Arc::new(self.definition().contract()))
        }
        async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
            lash_core::ToolOutcome::ok(call.args["text"].clone()).into()
        }
    }
    let host = open_host().await;
    let mut state = typescript_state();
    cell(&mut state, &host, "exec-code:0", Arc::new(EchoContract("first")),
        "async function shout(text) { return await echo.say({ text }); }\nconst alias = shout; const box = [shout];").await;
    let functions = state.lowering_functions();
    assert_eq!(
        functions.len(),
        1,
        "one environment is saved once despite aliases"
    );
    let old = functions
        .values()
        .next()
        .expect("the saved environment")
        .clone();
    let mut state = restored(&host, &mut state).await;
    let called = cell(
        &mut state,
        &host,
        "exec-code:1",
        Arc::new(EchoContract("second")),
        "await control.finish([await shout('a'), await alias('b'), await box[0]('c')]);",
    )
    .await;
    assert_eq!(finish_of(&called), serde_json::json!(["a", "b", "c"]));
    let functions = state.lowering_functions();
    let next = functions
        .values()
        .find(|function| function.environment == old.environment)
        .expect("the same saved environment");
    assert_ne!(next.link, old.link);
    assert_eq!(next.document, old.document);
    assert_eq!(next.environment, old.environment);
    assert_eq!(next.callable.function, old.callable.function);
}

/// FIG-7309: lexical and saved session identities own every builtin shortcut.
#[tokio::test(flavor = "multi_thread")]
async fn builtin_shadowing_never_selects_a_static_fast_path() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut state = typescript_state();
    for (key, code, expected) in [
        (
            "exec-code:0",
            r#"
const map = (x) => x + 10;
const JSON = { stringify: (x) => x + 20 };
const Math = { max: (x) => x + 30 };
const String = (x) => [x];
const Map = Array;
const NaN = "user"; const Infinity = "user"; const undefined = 5;
const finish = (x) => x + 40;
const control = (x) => x + 80;
console.log(control(1));
console.log(map(1), JSON.stringify(2) + 1, Math.max(3), finish(4));
function nested() { return [map(5), JSON.stringify(6) + 1, Math.max(7), finish(8)]; }
console.log(nested().join(","));
"#,
            vec!["81", "11 23 33 44", "15,27,37,48"],
        ),
        (
            "exec-code:1",
            r#"
console.log(map(1), JSON.stringify(2) + 1, Math.max(3), finish(4));
function nestedAgain() { return [map(5), JSON.stringify(6) + 1, Math.max(7), finish(8)]; }
console.log(nestedAgain().join(","));
const items = String(9); items.push(10); console.log(items.join(","));
const values = new Map(); values.push(11); console.log(values.join(","));
console.log(globalThis.JSON.stringify([12]));
const unusual = NaN; console.log(unusual + 1, Infinity + 1, undefined + 1);
function local(JSON, Math, String) { return [JSON.stringify(1), Math.max(2), String(3)]; }
console.log(local({stringify: (x) => x + 50}, {max: (x) => x + 60}, (x) => x + 70).join(","));
"#,
            vec![
                "11 23 33 44",
                "15,27,37,48",
                "9,10",
                "11",
                "[12]",
                "user1 user1 6",
                "51,62,73",
            ],
        ),
    ] {
        let response = cell(&mut state, &host, key, tools.clone(), code).await;
        assert_eq!(
            response
                .prints
                .iter()
                .map(|print| print.value.clone())
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|line| serde_json::json!(line))
                .collect::<Vec<_>>()
        );
        assert!(
            response.bindings.not_carried.is_empty(),
            "{:?}",
            response.bindings
        );
    }
}
