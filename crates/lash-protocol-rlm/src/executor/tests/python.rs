//! Python cells obey the same feedback, committed-state and print rules.

use super::*;

fn state() -> CodeModeExecutionState {
    crate::testing::session_state(&CellDialect::python())
}

fn python_services() -> super::super::CellServices {
    cell_services(&CellDialect::python(), python_workers())
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
        "import asyncio\nrender_invoice = asyncio.create_task(echo.say({'text': 'first'}))\nawait render_invoice",
    ).await;
    assert!(first.error().is_none(), "{:?}", first.error());
    assert_eq!(first.bindings.not_carried, ["render_invoice"]);
    // The turn's phase commit adopts what the parked cell committed.
    crate::testing::commit_working(&mut state, &host).await;
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
        // FIG-6678 names the admitting operation, independently of its
        // generated plan's nested calls.
        failure.message.contains("library function `py.mul`"),
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
            "await echo.say({'text': 'held'})",
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
        "await echo.say({'text': 'held'})",
    )
    .await;
    let failure = response.error().expect("the Once call was interrupted");
    assert_interrupted_effect_label(&failure.message, "echo.say");
    for text in [
        "tool_failed",
        "echo.say",
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
        .read(&archive.output.reference)
        .await
        .expect("read retained prints");
    let prints: Vec<serde_json::Value> =
        serde_json::from_slice(&bytes).expect("the print values, in order");
    assert_eq!(
        prints,
        [
            serde_json::json!("before"),
            serde_json::json!("x".repeat(size as usize)),
            serde_json::json!("after"),
        ]
    );
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
    state: &mut CodeModeExecutionState,
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
/// shape as well as the frozen captures.
#[tokio::test(flavor = "multi_thread")]
async fn a_saved_function_is_called_after_a_cold_restore() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut source = state();
    let code = "rate = 3\nasync def scale(value: int = 14) -> int:\n    return value * rate";
    saved_cell(&mut source, &host, "exec-code:0", tools.clone(), code).await;
    let held = source.lowering_functions();
    let written = held[&lash_vocabulary::names::Name::new("scale")]
        .written
        .as_ref()
        .expect("Python metadata");
    assert_eq!(written.signature, "async (value: int = 14) -> int");
    assert!(written.call.as_ref().expect("call shape").is_async);
    crate::testing::commit_working(&mut source, &host).await;
    let saved = source
        .hydrated_execution_graph()
        .await
        .expect("saved state");
    drop(source);
    let mut restored = state();
    restored
        .restore_execution_graph(&saved, lash_core::plugin::ExecutionGraphPurpose::Working)
        .expect("cold restore");
    let called = saved_cell(
        &mut restored,
        &host,
        "exec-code:1",
        tools,
        "await control.finish([await scale(), await scale(value=7)])",
    )
    .await;
    assert_eq!(finish_of(&called), serde_json::json!([42, 21]));
}

/// FIG-5778: a saved function's required effects are checked in the
/// calling environment before any code in the cell runs: restored into a
/// session that does not offer a tool it calls, it is refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_saved_function_is_refused_where_a_tool_it_calls_is_missing() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let mut source = state();
    saved_cell(&mut source, &host, "exec-code:0", tools.clone(), "async def shout(text):\n    return await echo.say({'text': text})\nonce = await shout('a')").await;
    assert!(
        source
            .lowering_functions()
            .contains_key(&lash_vocabulary::names::Name::new("shout")),
        "{:?}",
        source.not_carried()
    );
    crate::testing::commit_working(&mut source, &host).await;
    let saved = source
        .hydrated_execution_graph()
        .await
        .expect("saved state");
    let mut restored = state();
    restored
        .restore_execution_graph(&saved, lash_core::plugin::ExecutionGraphPurpose::Working)
        .expect("restore");
    let ctx = cell_context(
        &host,
        SESSION,
        TURN,
        "exec-code:1",
        Arc::new(super::saved_functions::NoTools),
    );
    let refused = run_cell(
        &mut restored,
        ctx.clone(),
        &python_services(),
        "await control.finish(await shout('b'))",
    )
    .await;
    assert!(refused.error().is_some(), "missing tool refuses the cell");
    super::saved_functions::assert_package_refusal(&ctx, false);
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
        "await control.finish([clamp(3), clamp(50), factor, limits['top']])",
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
            .not_carried()
            .get(&lash_vocabulary::names::Name::new("later")),
        Some(
            &lash_kernel_dialect::NotSaved::Capture {
                name: lash_vocabulary::names::Name::new("pending"),
                why: lash_kernel_dialect::CaptureRefusal::Task,
            }
            .to_string()
        )
    );
    let refused = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:1", tools),
        &python_services(),
        "await control.finish(later())",
    )
    .await;
    assert!(refused.error().is_some());
}

/// RLM-HISTORY-READ: Python uses the same Read surface and item/null shape.
#[tokio::test]
async fn rlm_history_read() {
    let host = open_host().await;
    let ctx = cell_context(
        &host,
        SESSION,
        TURN,
        "exec-code:0",
        Arc::new(CellTools::default()),
    )
    .with_turn_history(Default::default(), super::history_messages(3));
    let response = run_cell(&mut state(), ctx, &python_services(),
        "n = await rlm.history.length()\nfirst = await rlm.history.at(0)\nlast = await rlm.history.at(-1)\nabsent = await rlm.history.at(99)\ntail = await rlm.history.slice({'from': -2, 'to': 3})\nawait control.finish([n, first['id'], last['id'], absent, len(tail)])").await;
    assert!(response.error().is_none(), "{:?}", response.error());
    assert_eq!(
        finish_of(&response),
        serde_json::json!([3, "history-0", "latest", null, 2])
    );
}
