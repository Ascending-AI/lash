//! Python cells obey the same feedback, committed-state and print rules.

use super::*;

fn state() -> RlmExecutionState {
    let dialect = CellDialect::python();
    RlmExecutionState::new(dialect.name(), dialect.numbers())
}

fn python_services() -> super::super::CellServices {
    cell_services(&CellDialect::python(), python_workers(), None)
}

/// FIG-5803 / FIG-5779: Python reserves catalog namespace roots as well
/// as the complete callable names, before source or restored state can bind them.
#[tokio::test(flavor = "multi_thread")]
async fn catalog_namespace_roots_cannot_be_shadowed() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let services = python_services();
    for (key, name) in [
        ("exec-code:0", "control"),
        ("exec-code:1", "echo"),
        ("exec-code:2", "echo_say"),
    ] {
        let response = run_cell(
            &mut state(),
            cell_context(&host, SESSION, TURN, key, tools.clone()),
            &services,
            &format!("{name} = 1"),
        )
        .await;
        let error = response.error().expect("a tool name cannot be bound");
        assert!(error.message.contains("PY_SHADOWS_BUILTIN"), "{error:?}");
        assert!(error.message.contains(name), "{error:?}");
        assert!(error.message.contains(&format!("{name}_")), "{error:?}");
    }
    let mut restored = state();
    restored
        .patch_globals(
            &lash_rlm_types::RlmGlobalsPatchPluginBody {
                set_default: serde_json::Map::from_iter([("control".into(), serde_json::json!(1))]),
            },
            &std::collections::BTreeSet::new(),
        )
        .await
        .expect("restore a binding from before the namespace was offered");
    let response = run_cell(
        &mut restored,
        cell_context(&host, SESSION, TURN, "exec-code:3", tools),
        &services,
        "print(1)",
    )
    .await;
    let error = response
        .error()
        .expect("a restored root cannot mask the catalog");
    assert!(error.message.contains("PY_SHADOWS_BUILTIN"), "{error:?}");
}

/// FIG-5763 / K-SES-003 / ADR 0132 NR-2: failures preserve their typed
/// causes, bounds name library definitions, and interrupted Once calls
/// name admitted work and warn about its outside effects, also in Python.
#[tokio::test(flavor = "multi_thread")]
async fn failures_keep_their_causes_and_name_the_work() {
    let mut host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let services = python_services();
    let mut state = state();
    let first = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        &services,
        "import asyncio\nrender_invoice = asyncio.create_task(echo_say({'text': 'first'}))\nawait render_invoice",
    ).await;
    assert!(first.error().is_none(), "{:?}", first.error());
    assert_eq!(first.bindings.not_carried, ["render_invoice"]);
    let second = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:1", tools.clone()),
        &services,
        "raise ValueError('unsupported render_invoice in row 3')",
    )
    .await;
    let failure = second.error().expect("the raise ends the cell");
    assert_eq!(failure.defect, None);
    assert_eq!(failure.kind, lash_core::CellFailureKind::Program);
    assert!(
        failure
            .message
            .contains("ValueError: unsupported render_invoice in row 3"),
        "{}",
        failure.message
    );
    let refused = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:2", tools.clone()),
        &services,
        "print(render_invoice)",
    )
    .await;
    let failure = refused.error().expect("an unknown binding is refused");
    assert_eq!(failure.defect, None);
    assert!(
        failure.message.contains("PY_UNKNOWN_NAME"),
        "{}",
        failure.message
    );

    let bounded = super::super::CellServices {
        execution_bounds: crate::plugin::ExecutionBounds::unbounded()
            .with_memory_limit(crate::plugin::MemoryBound::logical_bytes(64 * 1024)),
        ..python_services()
    };
    let response = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:3", tools.clone()),
        &bounded,
        "payload = 'x' * 1000000",
    )
    .await;
    let failure = response
        .error()
        .expect("the repeat passes the memory bound");
    assert_eq!(failure.kind, lash_core::CellFailureKind::Policy);
    assert!(
        failure.message.contains("library function `text.repeat`"),
        "{}",
        failure.message
    );
    assert!(
        failure.message.contains("fewer or smaller values"),
        "{}",
        failure.message
    );

    tools.hold.store(true, Ordering::SeqCst);
    {
        let cell = run_cell(
            &mut state,
            cell_context(&host, SESSION, TURN, "exec-code:4", tools.clone()),
            &services,
            "await echo_say({'text': 'held'})",
        );
        tokio::select! {
            response = cell => panic!("held call ended: {:?}", response.result),
            () = tools.held_started.notified() => {}
        }
    }
    host.kill_and_resume().await;
    tools.hold.store(false, Ordering::SeqCst);
    let response = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:4", tools.clone()),
        &services,
        "await echo_say({'text': 'held'})",
    )
    .await;
    let failure = response.error().expect("the Once call was interrupted");
    for text in [
        "tool_failed",
        "echo_say",
        "site",
        "occurrence 0",
        "outside work may already have happened",
        "check outside state before calling again",
    ] {
        assert!(
            failure.message.contains(text),
            "missing {text}: {}",
            failure.message
        );
    }
    assert_eq!(tools.answered(), ["first"], "Once never starts again");
}

/// FIG-5766: accepted Python cells report added, changed, removed and
/// not-carried names; failed cells publish no binding transition.
#[tokio::test(flavor = "multi_thread")]
async fn committed_cells_report_binding_changes() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let services = python_services();
    let mut state = state();
    let first = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        &services,
        "kept = [1, 2]\ncount = 1\nscratch = 9",
    )
    .await;
    assert!(first.error().is_none(), "{:?}", first.error());
    assert_eq!(
        *first.bindings,
        lash_core::BindingChanges {
            added: vec!["count".into(), "kept".into(), "scratch".into()],
            ..Default::default()
        }
    );

    // A host projection replaces scratch with a read-only value. The
    // committed cell drops the session's copy and reports its removal.
    let ctx = cell_context(&host, SESSION, TURN, "exec-code:1", tools.clone())
        .with_recorded_render(crate::testing::recorded_test_render());
    let second = Box::pin(super::super::execute_cell(
        &mut state, ctx, lash_core::ExecRequest {
            code: "import asyncio\nrows = [len(kept)]\ncount = 2\njob = asyncio.create_task(echo_say({'text': 'second'}))\nawait job".into(),
        }, &services,
        crate::projection::RlmProjectedBindings::new()
            .bind_json("scratch", serde_json::json!(99)).expect("projected scratch"),
    )).await;
    assert!(second.error().is_none(), "{:?}", second.error());
    state.mark_code_execution_response_returned();
    state.accept_code_execution();
    assert_eq!(
        *second.bindings,
        lash_core::BindingChanges {
            added: vec!["rows".into()],
            changed: vec!["count".into()],
            removed: vec!["scratch".into()],
            not_carried: vec!["job".into()],
            ..Default::default()
        }
    );
    let failed = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:2", tools.clone()),
        &services,
        "count = 3\nlate = 1\nraise ValueError('stop')",
    )
    .await;
    assert!(failed.error().is_some());
    assert_eq!(*failed.bindings, lash_core::BindingChanges::default());
    let after = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:3", tools),
        &services,
        "print(count)",
    )
    .await;
    assert!(after.error().is_none(), "{:?}", after.error());
    assert_eq!(after.prints[0].value, serde_json::json!("2"));
    assert_eq!(*after.bindings, lash_core::BindingChanges::default());
}

/// FIG-1643: Python print uses the cell's ordered typed observations;
/// an oversized aggregate is retained as one complete attachment archive.
#[tokio::test(flavor = "multi_thread")]
async fn prints_are_ordered_and_retained_without_losing_values() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let services = python_services();
    let mut state = state();
    let inline = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        &services,
        "print('hello', 42)\nprint('second')",
    )
    .await;
    assert!(inline.error().is_none(), "{:?}", inline.error());
    assert_eq!(
        inline
            .prints
            .iter()
            .map(|print| print.text.as_str())
            .collect::<Vec<_>>(),
        ["hello 42", "second"]
    );
    assert_eq!(inline.prints[1].value, serde_json::json!("second"));
    assert!(inline.prints_retained.is_none());

    let ctx = cell_context(&host, SESSION, TURN, "exec-code:1", tools);
    let attachments = ctx.attachment_store();
    let size = attachments.output_retention().inline_limit_bytes + 1;
    let response = run_cell(
        &mut state,
        ctx,
        &services,
        &format!("print('before')\nprint('x' * {size})\nprint('after')"),
    )
    .await;
    assert!(response.error().is_none(), "{:?}", response.error());
    assert!(
        response.prints.is_empty(),
        "the retained archive replaces inline prints"
    );
    let archive = response
        .prints_retained
        .expect("the complete aggregate is retained");
    let bytes = attachments
        .read(&archive.reference)
        .await
        .expect("read retained prints");
    let prints: Vec<lash_core::CellPrint> =
        serde_json::from_slice(&bytes).expect("typed print archive");
    assert_eq!(prints.len(), 3);
    assert_eq!(prints[0].value, serde_json::json!("before"));
    assert_eq!(
        prints[1].value,
        serde_json::json!("x".repeat(size as usize))
    );
    assert_eq!(prints[2].value, serde_json::json!("after"));
}

/// FIG-5778: both def and lambda bindings survive two cell boundaries,
/// including calls to another saved function by name.
#[tokio::test(flavor = "multi_thread")]
async fn a_function_a_cell_defines_is_called_two_cells_later() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let services = python_services();
    let mut state = state();
    for (key, code) in [
        (
            "exec-code:0",
            "step = 2\ndef advance(value):\n    return value + step\ntwice = lambda value: advance(advance(value))",
        ),
        ("exec-code:1", "base = 38"),
    ] {
        let response = run_cell(
            &mut state,
            cell_context(&host, SESSION, TURN, key, tools.clone()),
            &services,
            code,
        )
        .await;
        assert!(response.error().is_none(), "{:?}", response.error());
    }
    let called = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:2", tools),
        &services,
        "print(twice(base))",
    )
    .await;
    assert!(called.error().is_none(), "{:?}", called.error());
    assert_eq!(called.prints[0].value, serde_json::json!("42"));
}

async fn saved_cell(
    state: &mut RlmExecutionState,
    host: &crate::testing::DurableHost,
    key: &'static str,
    tools: Arc<dyn lash_core::ToolProvider>,
    code: &str,
) -> lash_core::ExecResponse {
    let response = run_cell(
        state,
        cell_context(host, SESSION, TURN, key, tools),
        &python_services(),
        code,
    )
    .await;
    assert!(response.error().is_none(), "{:?}", response.error());
    response
}

/// FIG-5778: cold restore retains Python's keyword/default and async call
/// metadata as well as the frozen captures and original source.
#[tokio::test(flavor = "multi_thread")]
async fn a_saved_function_is_called_after_a_cold_restore() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut source = state();
    let code = "rate = 3\nasync def scale(value: int = 14) -> int:\n    return value * rate";
    saved_cell(&mut source, &host, "exec-code:0", tools.clone(), code).await;
    let held = source.bindings().held_functions();
    let written = held[&lash_kernel_doc::Name::new("scale")]
        .function
        .written
        .as_ref()
        .expect("Python metadata");
    assert_eq!(
        written.signature.as_deref(),
        Some("async (value: int = 14) -> int")
    );
    assert_eq!(
        written.source.as_deref(),
        Some(code.split_once('\n').expect("def text").1)
    );
    assert_eq!(
        written.metadata.as_ref().expect("call metadata")["is_async"],
        true
    );
    let fleet = lash_core::FleetFormat::current();
    source
        .snapshot_execution_state(fleet)
        .await
        .expect("snapshot");
    source.acknowledge_execution_state_capture();
    let saved = source
        .hydrated_execution_state(fleet)
        .await
        .expect("saved state");
    drop(source);
    let mut restored = state();
    restored
        .restore_execution_state(&saved, fleet)
        .await
        .expect("cold restore");
    let called = saved_cell(
        &mut restored,
        &host,
        "exec-code:1",
        tools,
        "await control_finish([await scale(), await scale(value=7)])",
    )
    .await;
    assert_eq!(finish_of(&called), serde_json::json!([42, 21]));
}

/// FIG-5778: seeding is an explicit creation input, and the new session
/// holds no data of the old session; a lambda keeps its keyword signature.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_session_created_with_a_saved_function_calls_it() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut source = state();
    saved_cell(
        &mut source,
        &host,
        "exec-code:0",
        tools.clone(),
        "greeting = 'hello'\ngreet = lambda name='lash': f'{greeting}, {name}'",
    )
    .await;
    let functions = source.saved_functions();
    assert_eq!(
        functions["greet"]["written"]["metadata"]["params"][0]["name"], "name",
        "{}",
        functions["greet"]
    );
    drop(source);
    let mut seeded = state();
    seeded
        .seed_functions(&functions, &std::collections::BTreeSet::new())
        .await
        .expect("seed functions");
    assert_eq!(seeded.binding_names().count(), 0);
    let called = saved_cell(
        &mut seeded,
        &host,
        "exec-code:1",
        tools,
        "await control_finish([greet(), greet(name='world')])",
    )
    .await;
    assert_eq!(
        finish_of(&called),
        serde_json::json!(["hello, lash", "hello, world"])
    );
}

/// FIG-5778: installing a function validates its required effects in the
/// calling environment before any code in the cell runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_saved_function_is_refused_where_a_tool_it_calls_is_missing() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut source = state();
    saved_cell(&mut source, &host, "exec-code:0", tools.clone(), "async def shout(text):\n    return await echo_say({'text': text})\nonce = await shout('a')").await;
    assert!(
        source.saved_functions().contains_key("shout"),
        "{:?}",
        source.bindings().not_carried()
    );
    let mut seeded = state();
    seeded
        .seed_functions(
            &source.saved_functions(),
            &std::collections::BTreeSet::new(),
        )
        .await
        .expect("seed");
    let refused = run_cell(
        &mut seeded,
        cell_context(
            &host,
            SESSION,
            TURN,
            "exec-code:1",
            Arc::new(super::saved_functions::NoTools),
        ),
        &python_services(),
        "await control_finish(await shout('b'))",
    )
    .await;
    let error = refused.error().expect("missing tool refuses the cell");
    assert!(
        error.message.contains("PY_SAVED_FUNCTION_UNUSABLE")
            && error.message.contains(
                "the saved function `shout` calls `echo_say`, which this session does not offer"
            ),
        "{}",
        error.message
    );
    assert_eq!(tools.answered(), ["a"]);
}

/// FIG-5778: later rebinding and mutation do not change frozen captures.
#[tokio::test(flavor = "multi_thread")]
async fn a_saved_function_keeps_the_captures_its_cell_left() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut state = state();
    saved_cell(&mut state, &host, "exec-code:0", tools.clone(), "factor = 2\nlimits = {'top': 10}\nclamp = lambda value: min(value * factor, limits['top'])").await;
    saved_cell(
        &mut state,
        &host,
        "exec-code:1",
        tools.clone(),
        "factor = 100\nlimits['top'] = 1000",
    )
    .await;
    let called = saved_cell(
        &mut state,
        &host,
        "exec-code:2",
        tools,
        "await control_finish([clamp(3), clamp(50), factor, limits['top']])",
    )
    .await;
    assert_eq!(finish_of(&called), serde_json::json!([6, 10, 100, 1000]));
}

/// FIG-5778 / K-SES-003: task captures cannot be frozen, even after await;
/// the binding is not carried and its recorded reason names the capture.
#[tokio::test(flavor = "multi_thread")]
async fn a_function_that_captures_a_task_is_not_carried_and_says_so() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut state = state();
    let defined = saved_cell(&mut state, &host, "exec-code:0", tools.clone(), "import asyncio\nasync def work():\n    return 1\npending = asyncio.create_task(work())\nawait pending\nlater = lambda: pending").await;
    assert_eq!(
        *defined.bindings,
        lash_core::BindingChanges {
            added: vec!["work".into()],
            not_carried: vec!["later".into(), "pending".into()],
            ..Default::default()
        }
    );
    assert_eq!(
        state
            .bindings()
            .not_carried()
            .get(&lash_kernel_doc::Name::new("later")),
        Some(&lash_kernel_dialect::NotSaved::Capture {
            name: lash_kernel_doc::Name::new("pending"),
            why: lash_kernel_dialect::CaptureRefusal::Task,
        })
    );
    let refused = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:1", tools),
        &python_services(),
        "await control_finish(later())",
    )
    .await;
    assert!(refused.error().is_some());
}

/// `ctx` running under a turn whose finish schema is `schema`: the RLM
/// namespace its run recorded, as the turn's driver reads it.
fn under_finish_schema(
    ctx: lash_core::RuntimeExecutionContext<'static>,
    schema: serde_json::Value,
) -> lash_core::RuntimeExecutionContext<'static> {
    let options = crate::plugin::RlmRecordedConfig::for_testing(lash_rlm_types::RlmTurnOptions {
        finish_schema: Some(lash_core::JsonSchema::admit(schema).expect("valid finish schema")),
        ..Default::default()
    });
    let mut config =
        lash_core::PluginConfig::for_protocol(Some(crate::plugin::RLM_PROTOCOL_PLUGIN_ID.into()));
    config.insert(
        crate::plugin::RLM_PROTOCOL_PLUGIN_ID,
        options
            .decode::<serde_json::Value>()
            .expect("the namespace is JSON"),
    );
    ctx.with_execution_env_spec(lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::new(config, 0),
        lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ),
        lash_core::SessionToolAccess::ambient(),
    ))
}

/// FIG-5802: `control_finish` takes its value under the turn's finish
/// schema as its input schema. A value the schema refuses fails the call
/// through ordinary input validation: the cell catches it and goes on, and
/// the call spent the cell's one control attempt, so a second
/// `control_finish` is refused before it runs.
#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_finish_value_fails_the_call_inside_the_cell() {
    let host = open_host().await;
    let mut state = state();
    let response = run_cell(
        &mut state,
        under_finish_schema(
            cell_context(
                &host,
                SESSION,
                TURN,
                "exec-code:0",
                Arc::new(CellTools::default()),
            ),
            serde_json::json!({ "type": "integer" }),
        ),
        &python_services(),
        "first = 'none'\ntry:\n    await control_finish('wrong')\nexcept Exception:\n    first = 'refused'\nsecond = 'none'\ntry:\n    await control_finish(7)\nexcept Exception:\n    second = 'refused'\nprint(first + ' then ' + second)",
    )
    .await;
    assert!(response.error().is_none(), "{:?}", response.error());
    assert_eq!(response.finish_value(), None, "no finish took effect");
    let printed: Vec<_> = response.prints.iter().map(|print| &print.value).collect();
    assert_eq!(printed, [&serde_json::json!("refused then refused")]);
}
