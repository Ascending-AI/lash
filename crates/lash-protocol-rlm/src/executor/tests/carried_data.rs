//! Functions anywhere in carried data carry to the next cell like named
//! ones (FIG-6927, SAVE-IN-DATA, SAVE-KEY, K-SES-002): every closure a
//! carried binding reaches, directly, inside data or through a capture, is
//! saved once, and every slot that held it holds a reference to it.

use std::sync::Arc;

use super::{CellTools, SESSION, TURN, finish_of, open_host};
use crate::CellDialect;
use crate::testing::{DurableHost, cell_context, cell_services, python_workers, run_cell};

/// A session of `dialect` whose cells must complete, numbered in order.
struct Session {
    host: DurableHost,
    state: super::super::CodeModeExecutionState,
    services: super::super::CellServices,
    tools: Arc<CellTools>,
    cells: usize,
}

impl Session {
    async fn open(dialect: &CellDialect) -> Self {
        let workers = if dialect.name() == CellDialect::python().name() {
            python_workers()
        } else {
            lash_vm_client::service::Service::default()
        };
        Self {
            host: open_host().await,
            state: crate::testing::session_state(dialect),
            services: cell_services(dialect, workers),
            tools: Arc::new((CellTools::default()).into()),
            cells: 0,
        }
    }

    async fn run(&mut self, code: &str) -> lash_core::ExecResponse {
        let key: &'static str = Box::leak(format!("exec-code:{}", self.cells).into_boxed_str());
        self.cells += 1;
        run_cell(
            &mut self.state,
            cell_context(&self.host, SESSION, TURN, key, self.tools.clone()),
            &self.services,
            code,
        )
        .await
    }

    async fn ok(&mut self, code: &str) -> lash_core::ExecResponse {
        let response = self.run(code).await;
        assert!(response.error().is_none(), "{:?}", response.error());
        response
    }

    /// The names of the functions the session holds.
    fn saved(&self) -> Vec<String> {
        self.state
            .lowering_functions()
            .keys()
            .map(|name| name.as_str().to_owned())
            .collect()
    }
}

/// SAVE-IN-DATA: a record holding a lambda in cell 1 calls it in cell 2,
/// and `const plan = {map: x => x}; const run = () => plan.map(1)` carries:
/// `run`'s capture of `plan` holds the lambda, and `run()` answers in
/// cell 2. Each dialect, in its own spelling.
#[tokio::test(flavor = "multi_thread")]
async fn a_function_held_in_data_or_a_capture_is_called_in_the_next_cell() {
    for (dialect, made, called) in [
        (
            CellDialect::typescript(),
            "const rec = { double: (x) => x * 2 };\nconst plan = { map: (x) => x };\nconst run = () => plan.map(1);",
            "await control.finish({ doubled: rec.double(4), ran: run(), mapped: plan.map(7) });",
        ),
        (
            CellDialect::python(),
            "rec = {'double': lambda x: x * 2}\nplan = {'map': lambda x: x}\nrun = lambda: plan['map'](1)",
            "await control.finish({'doubled': rec['double'](4), 'ran': run(), 'mapped': plan['map'](7)})",
        ),
    ] {
        let mut session = Session::open(&dialect).await;
        let made = session.ok(made).await;
        assert!(made.bindings.not_carried.is_empty(), "{:?}", made.bindings);
        assert_eq!(
            finish_of(&session.ok(called).await),
            serde_json::json!({ "doubled": 8, "ran": 1, "mapped": 7 }),
            "{}",
            dialect.name()
        );
    }
}

/// K-SES-002: two bindings that share a closure share it in the next cell.
/// `f`, held by its own binding, inside a record twice and inside a list,
/// is one saved function, `f`; a lambda two bindings reach through one
/// record, and a third record holds, is one saved function under a
/// generated name. Its frozen captures are cell 1's end values.
#[tokio::test(flavor = "multi_thread")]
async fn bindings_that_share_a_closure_share_one_saved_function_in_the_next_cell() {
    for (dialect, made, read) in [
        (
            CellDialect::typescript(),
            "let step = 1;\nconst f = (x) => x + step;\nconst pair = { f, again: f };\nconst list = [f];\nconst a = { g: (x) => x };\nconst b = a;\nconst c = { g: a.g };\nstep = 2;",
            "await control.finish({ pair: pair.f === f, again: pair.again === f, listed: list[0] === f, shared: b.g === a.g, copied: c.g === a.g, record: b === a, called: pair.f(1), listedCall: list[0](2) });",
        ),
        (
            CellDialect::python(),
            "step = 1\nf = lambda x: x + step\npair = {'f': f, 'again': f}\nlisted = [f]\na = {'g': lambda x: x}\nb = a\nc = {'g': a['g']}\nstep = 2",
            "await control.finish({'pair': pair['f'] is f, 'again': pair['again'] is f, 'listed': listed[0] is f, 'shared': b['g'] is a['g'], 'copied': c['g'] is a['g'], 'record': b is a, 'called': pair['f'](1), 'listedCall': listed[0](2)})",
        ),
    ] {
        let mut session = Session::open(&dialect).await;
        session.ok(made).await;
        assert_eq!(session.saved(), ["f", "saved#0"], "{}", dialect.name());
        assert_eq!(
            finish_of(&session.ok(read).await),
            serde_json::json!({
                "pair": true, "again": true, "listed": true, "shared": true,
                "copied": true, "record": true, "called": 3, "listedCall": 4,
            }),
            "{}",
            dialect.name()
        );
    }
}

/// SAVE-IN-DATA keeps the refusals it does not lift: a binding whose data
/// holds a task, or a function a library body made, is not carried, and
/// says which.
#[tokio::test(flavor = "multi_thread")]
async fn data_holding_a_task_or_a_library_made_function_is_not_carried_and_says_so() {
    let mut session = Session::open(&CellDialect::typescript()).await;
    let made = session
        .ok("const tasks = { p: (async () => 1)() };\nconst bound = { b: ((x) => x).bind(null) };\nconst kept = { k: (x) => x };")
        .await;
    assert_eq!(made.bindings.not_carried, ["bound", "tasks"]);
    let not_carried = session.state.not_carried();
    assert_eq!(
        not_carried[&lash_kernel_doc::Name::new("tasks")],
        lash_kernel_dialect::NotSaved::Task.to_string()
    );
    assert_eq!(
        not_carried[&lash_kernel_doc::Name::new("bound")],
        lash_kernel_dialect::NotSaved::MadeByLibrary.to_string()
    );
    assert_eq!(
        finish_of(&session.ok("await control.finish(kept.k(5));").await),
        serde_json::json!(5)
    );
}
