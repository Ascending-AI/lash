//! Python cells obey the same feedback, committed-state and print rules.

use super::*;

fn state() -> RlmExecutionState {
    let dialect = CellDialect::python();
    RlmExecutionState::new(dialect.name(), dialect.numbers())
}

fn python_services() -> super::super::CellServices {
    cell_services(&CellDialect::python(), python_workers(), None)
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
