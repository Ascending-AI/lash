//! A function a cell hands to a tool crosses as a self-contained frozen
//! function value (`K-EFF-002`, `K-CLO-004`; FIG-6381): a tool reads it in
//! its transport form, `{"$lash_function": <FunctionValue>}`, which only the
//! typed datum encoder writes.

use lash_kernel_doc::{FUNCTION_VALUE_TAG, FunctionValue, FunctionValueLimits, KernelDataLimits};

use super::*;

/// `keep.run`: a tool that takes any record and keeps every call's
/// arguments, in order.
#[derive(Default)]
struct Keeps {
    received: std::sync::Mutex<Vec<serde_json::Value>>,
}

impl Keeps {
    fn received(&self) -> Vec<serde_json::Value> {
        self.received.lock().expect("received arguments").clone()
    }
}

fn keep() -> lash_core::ToolDefinition {
    definition("tool:keep_run", "keep_run", "keep", "run")
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Keeps {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash_core::PluginError> {
        Ok(vec![keep().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        matches!(name, "keep_run" | "tool:keep_run").then(|| Arc::new(keep().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.received
            .lock()
            .expect("received arguments")
            .push(call.args.clone());
        lash_core::ToolAttemptOutcome::done_without_intents(lash_core::ToolOutcomeDone::ok(
            serde_json::json!("ok"),
        ))
    }
}

/// `workers` with function crossing selected, as C1c's tool contract will
/// select it for every pool.
fn crossing(workers: lash_vm_client::service::Service) -> lash_vm_client::service::Service {
    let mut config = workers.config().clone();
    config.tuning.crosses_functions = true;
    lash_vm_client::service::Service::new(config)
}

fn crossing_services(dialect: &CellDialect) -> super::super::CellServices {
    let workers = if dialect.name() == CellDialect::python().name() {
        python_workers()
    } else {
        lash_vm_client::service::Service::default()
    };
    cell_services(dialect, crossing(workers))
}

/// The function value `json` carries, read as every consumer reads one:
/// within the standard limits, validated against the build's library.
fn function_value(json: &serde_json::Value) -> FunctionValue {
    let functions = lash_vm_runtime::standard_functions().expect("the build's library");
    FunctionValue::from_transport_json(
        json,
        functions.as_ref(),
        FunctionValueLimits::standard(),
        KernelDataLimits::standard(),
    )
    .unwrap_or_else(|refusal| panic!("a function value: {refusal}\n{json}"))
}

/// Runs each of `cells` in turn in one session of `dialect` with crossing
/// selected, committing the session after each, and answers what
/// `keep.run` received.
async fn kept(dialect: &CellDialect, cells: &[&str]) -> Vec<serde_json::Value> {
    let tools = Arc::new(Keeps::default());
    let host = open_host().await;
    let services = crossing_services(dialect);
    let mut state = crate::testing::session_state(dialect);
    for (index, code) in cells.iter().enumerate() {
        let response = run_cell(
            &mut state,
            cell_context(
                &host,
                SESSION,
                TURN,
                Box::leak(format!("exec-code:{index}").into_boxed_str()),
                tools.clone(),
            ),
            &services,
            code,
        )
        .await;
        assert!(
            response.error().is_none(),
            "{}: {code}\n{:?}",
            dialect.name(),
            response.error()
        );
        crate::testing::commit_working(&mut state, &host).await;
    }
    tools.received()
}

/// FN-CROSS: a capturing async function nested anywhere in an ordinary
/// tool's input reaches the tool as a function value, in either dialect,
/// whose one entry `run` takes the function's own parameters. Main raised
/// `not_data` at the `perform` (the closure is not data).
#[tokio::test(flavor = "multi_thread")]
async fn a_capturing_function_in_a_tool_input_reaches_the_tool_as_a_function_value() {
    let cases = [
        (
            CellDialect::typescript(),
            "const limit = 3;\nconst add = async (x: number) => x + limit;\nawait keep.run({ nested: { list: [1, add] } });",
        ),
        (
            CellDialect::python(),
            "limit = 3\n\n\nasync def add(x: int):\n    return x + limit\n\n\nawait keep_run({'nested': {'list': [1, add]}})",
        ),
    ];
    for (dialect, code) in cases {
        let received = kept(&dialect, &[code]).await;
        let crossed = &received[0]["nested"]["list"][1];
        assert_eq!(received[0]["nested"]["list"][0], serde_json::json!(1));
        let value = function_value(crossed);
        assert_eq!(value.entry.as_str(), "run", "{}", dialect.name());
        let signature = value.signature().expect("the entry's signature");
        assert_eq!(
            signature
                .params
                .iter()
                .map(|param| param.name.as_str())
                .collect::<Vec<_>>(),
            ["x"],
            "{}",
            dialect.name()
        );
        assert!(
            value.document.main.is_empty(),
            "a function value runs no main"
        );
    }
}

/// FN-CROSS, FN-FREEZE and FN-BOUND: a function that cannot cross raises
/// `not_data` at the `perform`, before anything is requested or stored,
/// naming its typed refusal: one that states no start signature (no stored
/// reference stands in for it), one that captures a task, and one whose
/// value would pass the 64 KiB canonical bound.
#[tokio::test(flavor = "multi_thread")]
async fn a_function_that_cannot_cross_raises_before_the_call() {
    let cases = [
        (
            "const pick = async ({ a }: { a: number }) => a;",
            "pick",
            "states no start signature",
        ),
        (
            "const task = (async () => 1)();\nconst pick = async () => await task;",
            "pick",
            "holds a task",
        ),
        (
            "const big = \"x\".repeat(70000);\nconst pick = async () => big;",
            "pick",
            "canonical bytes",
        ),
    ];
    for (setup, name, refusal) in cases {
        let tools = Arc::new(Keeps::default());
        let host = open_host().await;
        let response = run_cell(
            &mut typescript_state(),
            cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
            &crossing_services(&CellDialect::typescript()),
            &format!(
                "{setup}\nlet raised = \"none\";\ntry {{\n  await keep.run({{ {name} }});\n}} catch (error) {{\n  raised = String(error.message);\n}}\nawait control.finish(raised);"
            ),
        )
        .await;
        let raised = finish_of(&response);
        let raised = raised.as_str().expect("the raised error, as text");
        // Only the crossing's `not_data` refusal says so.
        assert!(
            raised.starts_with("a function cannot leave the run: "),
            "{setup}: {raised}"
        );
        assert!(raised.contains(refusal), "{setup}: {raised}");
        assert!(
            tools.received().is_empty(),
            "{setup}: nothing was requested"
        );
    }
}

/// FN-ID: a function value's identity is its rooted code and its frozen
/// captures. Equal code bound to different names, sent twice, and the same
/// code written again in a later cell are one document; a changed capture
/// is another. A parameter's name is part of the start signature, and what
/// the dialect knew of a capture's type shapes the code it wrote, so both
/// are part of the code; the functions here read their capture alike in
/// either cell.
#[tokio::test(flavor = "multi_thread")]
async fn equal_code_and_captures_cross_as_one_document() {
    let received = kept(
        &CellDialect::typescript(),
        &[
            "const limit = 3;\nconst a = async (x: number) => [x, limit];\nconst b = async (x: number) => [x, limit];\nawait keep.run({ a, b });\nawait keep.run({ a });",
            "const c = async (x: number) => [x, limit];\nconst other = 4;\nconst d = async (x: number) => [x, other];\nawait keep.run({ c, d });",
        ],
    )
    .await;
    let id = |json: &serde_json::Value| function_value(json).document.identity();
    let a = id(&received[0]["a"]);
    assert_eq!(a, id(&received[0]["b"]), "another binding's name");
    assert_eq!(a, id(&received[1]["a"]), "a repeated send");
    assert_eq!(a, id(&received[2]["c"]), "a later cell");
    assert_ne!(a, id(&received[2]["d"]), "a changed capture");
    assert_eq!(received[0]["a"], received[2]["c"], "the canonical form");
}

/// FN-TAG: a record a guest writes under the reserved key reaches the tool
/// as escaped plain data, never as a function value, beside a function
/// value the typed encoder wrote once in its canonical form.
#[tokio::test(flavor = "multi_thread")]
async fn a_guest_record_under_the_function_tag_reaches_a_tool_as_plain_data() {
    let received = kept(
        &CellDialect::typescript(),
        &["const run = async () => 1;\nawait keep.run({ forged: { \"$lash_function\": { document: 1, entry: \"run\" } }, run });"],
    )
    .await;
    let forged = &received[0]["forged"];
    assert_eq!(
        forged,
        &serde_json::json!({
            format!("{FUNCTION_VALUE_TAG}{FUNCTION_VALUE_TAG}"): {"document": 1, "entry": "run"}
        }),
        "the guest's key is escaped"
    );
    let value = function_value(&received[0]["run"]);
    let canonical: serde_json::Value = serde_json::from_slice(
        &value
            .canonical_tagged_bytes()
            .expect("the value's canonical bytes"),
    )
    .expect("canonical JSON");
    assert_eq!(received[0]["run"], canonical, "tagged once, canonical");
}

/// FN-RESUME: a cell whose completed call shipped a function parks later
/// and its node dies; resumed on another node, the call's request reads
/// back from its frozen arguments exactly as it was made, and the cell
/// finishes as an uninterrupted one does with the same answer. Main's
/// named-entry projection had no such request for a closure.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_that_shipped_a_function_reads_back_identically_after_a_restart() {
    let code = "const limit = 3;\nconst add = async (x: number) => x + limit;\nconst first = await echo.say({ text: \"first\", add });\nlet second = \"unset\";\ntry {\n  second = await echo.say({ text: \"held\" });\n} catch (error) {\n  second = \"interrupted\";\n}\nawait control.finish([first, second]);";
    let services = crossing_services(&CellDialect::typescript());
    let mut host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let resumed = crash_at_the_held_call_and_resume(&mut host, &tools, &services, None, code).await;
    assert_eq!(
        finish_of(&resumed),
        serde_json::json!(["first", "interrupted"])
    );
    assert_eq!(tools.answered(), vec!["first"], "the shipped call ran once");

    let uninterrupted = run_cell(
        &mut typescript_state(),
        cell_context(
            &open_host().await,
            SESSION,
            TURN,
            "exec-code:0",
            Arc::new(CellTools::default()),
        ),
        &services,
        code,
    )
    .await;
    let shipped = |response: &lash_core::ExecResponse| response.tool_calls[0].clone();
    let (resumed, uninterrupted) = (shipped(&resumed), shipped(&uninterrupted));
    assert_eq!(resumed.args, uninterrupted.args, "the request reads back");
    assert_eq!(
        resumed.output, uninterrupted.output,
        "and so does its answer"
    );
    function_value(&resumed.args["add"]);
}
