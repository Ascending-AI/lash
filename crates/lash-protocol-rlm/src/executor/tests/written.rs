//! FIG-6375: what a cell's source wrote of each function it makes is a
//! typed record, stored with the cell, that a park and a resume on another
//! node keep, and that needs no map of where the statements came from.

use super::*;

#[derive(Clone, Copy, Debug)]
enum Lang {
    TypeScript,
    Python,
}

impl Lang {
    fn dialect(self) -> CellDialect {
        match self {
            Self::TypeScript => CellDialect::typescript(),
            Self::Python => CellDialect::python(),
        }
    }

    fn state(self) -> CodeModeExecutionState {
        crate::testing::session_state(&self.dialect())
    }

    /// The services of a session of this language whose worker lowers with
    /// its source map when `mapped`, and with none otherwise.
    fn services(self, mapped: bool) -> super::super::CellServices {
        let workers = match (self, mapped) {
            (_, false) => crate::testing::unmapped_workers(),
            (Self::TypeScript, true) => lash_vm_client::service::Service::default(),
            (Self::Python, true) => python_workers(),
        };
        cell_services(&self.dialect(), workers)
    }

    /// A first cell that makes a named async function, an anonymous one
    /// bound later and a lambda, then waits on the held call.
    fn defining_cell(self) -> &'static str {
        match self {
            Self::TypeScript => {
                "const rate = 3;\nasync function scale(x: number, extra: number): Promise<number> { return x * rate + extra; }\nconst twice = async (y: number) => y * 2;\nconst plus = (z) => z + rate;\ntry {\n  await echo.say({ text: \"held\" });\n} catch (error) {}"
            }
            Self::Python => {
                "rate = 3\nasync def scale(x: int, extra: int = 1) -> int:\n    return x * rate + extra\ntwice = lambda y: y * 2\nasync def plus(z: int) -> int:\n    return z + rate\ntry:\n    await echo.say({'text': 'held'})\nexcept Exception:\n    pass"
            }
        }
    }

    /// A later cell that calls each function `defining_cell` saved.
    fn calling_cell(self) -> &'static str {
        match self {
            Self::TypeScript => {
                "await control.finish([await scale(2, 1), await twice(5), plus(4)]);"
            }
            Self::Python => {
                "await control.finish([await scale(2, extra=1), twice(5), await plus(4)])"
            }
        }
    }
}

/// Runs `code` as the session's first cell until its held call starts,
/// kills the node with the call running, and resumes the cell on another
/// node, where it runs to its end. Answers the session the resumed cell
/// left.
async fn parked_and_resumed(
    host: &mut DurableHost,
    lang: Lang,
    services: &super::super::CellServices,
    tools: &Arc<CellTools>,
) -> CodeModeExecutionState {
    let code = lang.defining_cell();
    tools.hold.store(true, Ordering::SeqCst);
    {
        let mut state = lang.state();
        let cell = run_cell(
            &mut state,
            cell_context(host, SESSION, TURN, "exec-code:0", tools.clone()),
            services,
            code,
        );
        tokio::select! {
            response = cell => panic!("the cell ended with its call held: {:?}", response.result),
            () = tools.held_started.notified() => {}
        }
    }
    host.kill_and_resume().await;
    tools.hold.store(false, Ordering::SeqCst);
    let mut state = lang.state();
    let resumed = run_cell(
        &mut state,
        cell_context(host, SESSION, TURN, "exec-code:0", tools.clone()),
        services,
        code,
    )
    .await;
    assert!(resumed.error().is_none(), "{lang:?}: {:?}", resumed.error());
    // The turn's phase commit roots the session the resumed cell left, as
    // it does before any later cell reads it.
    crate::testing::commit_working(&mut state, host).await;
    state
}

/// The functions `state` holds, by binding, as the session hands them to
/// its next cell's lowering.
fn saved(state: &CodeModeExecutionState) -> Vec<(String, lash_vm_client::service::LowerFunction)> {
    state
        .lowering_functions()
        .into_iter()
        .map(|(name, function)| (name.to_string(), function))
        .collect()
}

/// `functions` as data: what two sessions compare equal by.
fn as_data(functions: &[(String, lash_vm_client::service::LowerFunction)]) -> serde_json::Value {
    serde_json::to_value(functions).expect("saved functions encode")
}

/// WR-PARK: a cell parked on a call, killed with its node and resumed on
/// another saves the same functions, each with what its source wrote of
/// it, as the same cell run without interruption; a later cell calls them
/// alike.
#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_cell_saves_the_functions_an_uninterrupted_one_does() {
    for lang in [Lang::TypeScript, Lang::Python] {
        let services = lang.services(true);
        let tools = Arc::new(CellTools::default());
        let mut host = open_host().await;
        let mut resumed = parked_and_resumed(&mut host, lang, &services, &tools).await;

        let straight_host = open_host().await;
        let mut straight = lang.state();
        let ran = run_cell(
            &mut straight,
            cell_context(&straight_host, SESSION, TURN, "exec-code:0", tools.clone()),
            &services,
            lang.defining_cell(),
        )
        .await;
        assert!(ran.error().is_none(), "{lang:?}: {:?}", ran.error());
        crate::testing::commit_working(&mut straight, &straight_host).await;

        let kept = saved(&resumed);
        assert_eq!(
            kept.iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["plus", "scale", "twice"],
            "{lang:?}"
        );
        assert!(
            kept.iter().all(|(_, function)| function.written.is_some()),
            "{lang:?}: every saved function keeps what its source wrote: {kept:#?}"
        );
        assert_eq!(as_data(&kept), as_data(&saved(&straight)), "{lang:?}");

        let mut answers = Vec::new();
        for (state, host) in [(&mut resumed, &host), (&mut straight, &straight_host)] {
            let called = run_cell(
                state,
                cell_context(host, SESSION, TURN, "exec-code:1", tools.clone()),
                &services,
                lang.calling_cell(),
            )
            .await;
            answers.push(finish_of(&called));
        }
        assert_eq!(answers[0], serde_json::json!([7, 10, 7]), "{lang:?}");
        assert_eq!(answers[0], answers[1], "{lang:?}");
    }
}

/// WR-CALL: after a park and a resume, a later Python cell places keyword
/// arguments by the saved function's parameters and refuses a coroutine
/// it does not await, the same with the cell's source map as without it.
#[tokio::test(flavor = "multi_thread")]
async fn a_later_cell_calls_a_resumed_cells_function_alike_with_or_without_a_map() {
    let mut outcomes = Vec::new();
    for mapped in [true, false] {
        let services = Lang::Python.services(mapped);
        let tools = Arc::new(CellTools::default());
        let mut host = open_host().await;
        let mut state = parked_and_resumed(&mut host, Lang::Python, &services, &tools).await;
        let placed = run_cell(
            &mut state,
            cell_context(&host, SESSION, TURN, "exec-code:1", tools.clone()),
            &services,
            "await control.finish([await scale(2, extra=4), await scale(extra=0, x=5), await scale(1)])",
        )
        .await;
        crate::testing::commit_working(&mut state, &host).await;
        let refused = run_cell(
            &mut state,
            cell_context(&host, SESSION, TURN, "exec-code:2", tools.clone()),
            &services,
            "pending = plus(1)",
        )
        .await;
        let refusal = refused
            .error()
            .map(|error| error.message.clone())
            .unwrap_or_default();
        assert!(
            refusal.contains("PY_COROUTINE_NOT_AWAITED"),
            "mapped {mapped}: {refusal}"
        );
        outcomes.push((finish_of(&placed), refusal));
    }
    assert_eq!(outcomes[0].0, serde_json::json!([10, 15, 4]));
    assert_eq!(outcomes[0], outcomes[1]);
}
