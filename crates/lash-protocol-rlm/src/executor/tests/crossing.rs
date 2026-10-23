//! A function a cell hands to a tool crosses as a self-contained frozen
//! function value (`K-EFF-002`, `K-CLO-004`; FIG-6381) wherever the tool's
//! schema marks the slot `{"x-lash": {"kind": "function"}}`: a tool reads it
//! in its transport form, `{"$lash_function": <FunctionValue>}`, which only
//! the typed datum encoder writes. Every other JSON-schema slot is data, and
//! a function there raises `not_data` naming its path.

use lash_kernel_doc::FUNCTION_VALUE_TAG;
use lash_kernel_doc::FunctionValue;
use lash_kernel_doc::FunctionValueLimits;
use lash_vocabulary::data_limits::KernelDataLimits;

use super::*;

/// `keep.run`: a tool that keeps every call's arguments, in order. Its
/// input marks each slot a law passes a function in; `forged` is data.
struct Keeps {
    tool: fn() -> lash_vocabulary::tool_contract::ToolDefinition,
    received: std::sync::Mutex<Vec<serde_json::Value>>,
}

impl Default for Keeps {
    fn default() -> Self {
        Self::of(keep)
    }
}

impl Keeps {
    fn of(tool: fn() -> lash_vocabulary::tool_contract::ToolDefinition) -> Self {
        Self {
            tool,
            received: std::sync::Mutex::default(),
        }
    }

    fn received(&self) -> Vec<serde_json::Value> {
        self.received.lock().expect("received arguments").clone()
    }
}

fn keep() -> lash_vocabulary::tool_contract::ToolDefinition {
    let function = serde_json::json!({ "x-lash": { "kind": "function" } });
    let mut properties = serde_json::Map::new();
    for name in ["pick", "a", "b", "c", "d", "run"] {
        properties.insert(name.to_owned(), function.clone());
    }
    properties.insert(
        "nested".to_owned(),
        serde_json::json!({
            "type": "object",
            "properties": { "list": { "type": "array", "items": function } },
            "additionalProperties": false
        }),
    );
    properties.insert("forged".to_owned(), serde_json::json!({ "type": "object" }));
    typed_definition(
        "tool:keep_run",
        "keep.run",
        serde_json::json!({
            "type": "object",
            "properties": properties,
            "additionalProperties": false
        }),
    )
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Keeps {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(vec![(self.tool)().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        let tool = (self.tool)();
        (name == tool.manifest.tool() || name == tool.id().as_str())
            .then(|| Arc::new(tool.contract()))
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

fn crossing_services(dialect: &CellDialect) -> super::super::CellServices {
    let workers = if dialect.name() == CellDialect::python().name() {
        python_workers()
    } else {
        lash_vm_client::service::Service::default()
    };
    cell_services(dialect, workers)
}

/// The function value `json` carries, read as every consumer reads one:
/// within the standard limits, validated against the build's library.
fn function_value(json: &serde_json::Value) -> FunctionValue {
    let functions = lash_vm_releases::helper_release()
        .map(|release| std::sync::Arc::new(release.functions.clone()))
        .expect("the build's library");
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
    kept_with_host(dialect, cells).await.0
}
async fn kept_with_host(
    dialect: &CellDialect,
    cells: &[&str],
) -> (Vec<serde_json::Value>, DurableHost) {
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
    (tools.received(), host)
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
            "limit = 3\n\n\nasync def add(x: int):\n    return x + limit\n\n\npayload = {'nested': {'list': [1, add]}}\nawait keep.run(payload)",
        ),
    ];
    for (dialect, code) in cases {
        let received = kept(&dialect, &[code]).await;
        let crossed = &received[0]["nested"]["list"][1];
        assert_eq!(received[0]["nested"]["list"][0], serde_json::json!(1));
        let value = function_value(crossed);
        let written = value
            .written_signature
            .as_ref()
            .expect("K-CALLABLE-PRESENTATION: crossing retains source spelling");
        assert!(!written.signature.is_empty());
        assert_eq!(
            written.language,
            if dialect.name() == CellDialect::python().name() {
                lash_kernel_doc::SourceLanguage::Python
            } else {
                lash_kernel_doc::SourceLanguage::TypeScript
            }
        );
        let signature = value.signature();
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
    }
    // A later cell ships the saved environment with the defining source's
    // coordinates, rather than the current cell's spelling or vector index.
    let source = "const plan = (event) => event;";
    let received = kept(
        &CellDialect::typescript(),
        &[source, "await keep.run({pick: plan});"],
    )
    .await;
    let value = function_value(&received[0]["pick"]);
    assert!(
        value
            .written_signature
            .as_ref()
            .unwrap()
            .signature
            .contains("event")
    );
    let origin = value
        .signature_origin
        .expect("saved annotation coordinates");
    assert_eq!(origin.name.as_deref(), Some("plan"));
    let span = origin.params[0].span.unwrap();
    assert_eq!(&source[span.start..span.end], "event");
}

/// FN-CROSS, FN-FREEZE and FN-BOUND: a function that cannot cross raises
/// `not_data` at the `perform`, before anything is requested or stored,
/// naming its typed refusal: one that captures a task, and one whose
/// value would pass the 64 KiB canonical bound.
#[tokio::test(flavor = "multi_thread")]
async fn a_function_that_cannot_cross_raises_before_the_call() {
    let cases = [
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

/// `slots.take`: a tool shaped like `processes.start`: a function slot the
/// `x-lash` marker declares, the same marker under `anyOf` beside null (as
/// pydantic and MCP spell an optional function), an open object, a text
/// and a closed object.
fn slots() -> lash_vocabulary::tool_contract::ToolDefinition {
    typed_definition(
        "tool:slots_take",
        "slots.take",
        serde_json::json!({
            "type": "object",
            "properties": {
                "definition": { "x-lash": { "kind": "function" } },
                "fallback": {
                    "anyOf": [{ "x-lash": { "kind": "function" } }, { "type": "null" }]
                },
                "args": { "type": "object" },
                "label": { "type": "string" },
                "plan": {
                    "type": "object",
                    "properties": { "step": { "type": "string" } },
                    "additionalProperties": false
                }
            },
            "additionalProperties": false
        }),
    )
}

/// `model.take`: a tool whose input is what pydantic writes for a model
/// with an `Optional[Callable]` field and a field of a marked `$defs` type:
/// an open object (no `additionalProperties`), as MCP servers write theirs
/// too.
fn open_take() -> lash_vocabulary::tool_contract::ToolDefinition {
    typed_definition(
        "tool:model_take",
        "model.take",
        serde_json::json!({
            "$defs": {
                "Handler": { "x-lash": { "kind": "function" }, "title": "Handler" }
            },
            "properties": {
                "callback": {
                    "anyOf": [{ "x-lash": { "kind": "function" } }, { "type": "null" }],
                    "default": null,
                    "title": "Callback"
                },
                "handler": { "$ref": "#/$defs/Handler" },
                "label": { "title": "Label", "type": "string" }
            },
            "required": ["label"],
            "title": "TakeInput",
            "type": "object"
        }),
    )
}

/// What `slots.take(input)` raised, as text, `input` written in `dialect`
/// and handed through a helper, so only the perform knows what it holds,
/// and what the tool received.
async fn slots_raised(dialect: &CellDialect, input: &str) -> (String, Vec<serde_json::Value>) {
    take_raised(slots, "slots", dialect, input).await
}

/// What `<module>.take(input)` raised, as [`slots_raised`] does for
/// `slots.take`, `tool` being that operation.
async fn take_raised(
    tool: fn() -> lash_vocabulary::tool_contract::ToolDefinition,
    module: &str,
    dialect: &CellDialect,
    input: &str,
) -> (String, Vec<serde_json::Value>) {
    let code = if dialect.name() == CellDialect::python().name() {
        format!(
            "async def f():\n    return 1\n\n\nasync def take(value):\n    return await {module}.take(value)\n\n\nraised = 'none'\ntry:\n    await take({input})\nexcept Exception as error:\n    raised = str(error)\nawait control.finish(raised)"
        )
    } else {
        format!(
            "const f = async () => 1;\nconst take = async (input) => await {module}.take(input);\nlet raised = \"none\";\ntry {{\n  await take({input});\n}} catch (error) {{\n  raised = String(error.message);\n}}\nawait control.finish(raised);"
        )
    };
    let tools = Arc::new(Keeps::of(tool));
    let host = open_host().await;
    let response = run_cell(
        &mut crate::testing::session_state(dialect),
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        &crossing_services(dialect),
        &code,
    )
    .await;
    let raised = finish_of(&response);
    (
        raised
            .as_str()
            .unwrap_or_else(|| panic!("{code}: the raised error, as text: {raised}"))
            .to_owned(),
        tools.received(),
    )
}

/// CROSS-BY-TYPE: a function crosses into a tool exactly where the tool's
/// schema marks the slot with the `x-lash` function kind, as
/// `processes.start`'s `definition` is.
#[tokio::test(flavor = "multi_thread")]
async fn a_function_crosses_in_a_slot_the_x_lash_function_marker_declares() {
    for (dialect, input) in [
        (
            CellDialect::typescript(),
            "{ definition: f, args: { n: 1 } }",
        ),
        (CellDialect::python(), "{'definition': f, 'args': {'n': 1}}"),
    ] {
        let (raised, received) = slots_raised(&dialect, input).await;
        assert_eq!(raised, "none", "{}", dialect.name());
        assert_eq!(received[0]["args"], serde_json::json!({"n": 1}));
        function_value(&received[0]["definition"]);
    }
}

/// X-LASH-ONE under composition (FIG-6951): a slot spelled
/// `anyOf: [{x-lash function}, {type: null}]` is presented as a function or
/// null in either dialect, and takes either: a function crosses into it as
/// a function value, and null reaches the tool as null. Main typed every
/// composition `Json`, so the function raised `not_data` there.
#[tokio::test(flavor = "multi_thread")]
async fn a_function_crosses_in_a_nullable_slot_the_x_lash_marker_declares_under_any_of() {
    for (dialect, function, null, callable, none) in [
        (
            CellDialect::typescript(),
            "{ fallback: f }",
            "{ fallback: null }",
            "=> Promise<unknown>",
            "null",
        ),
        (
            CellDialect::python(),
            "{'fallback': f}",
            "{'fallback': None}",
            "Callable[..., Any]",
            "None",
        ),
    ] {
        let signature = crate::loaded_tool_signature(&dialect, &serde_json::json!({}), &slots())
            .expect("an addressable tool");
        // The slot's spelling: from its name to its null member.
        let slot = signature
            .split_once("fallback")
            .and_then(|(_, rest)| rest.split_once(none))
            .map(|(slot, _)| slot)
            .unwrap_or_else(|| panic!("{signature}: the slot is presented nullable"));
        assert!(slot.contains(callable), "{}: {signature}", dialect.name());
        let (raised, received) = slots_raised(&dialect, function).await;
        assert_eq!(raised, "none", "{}", dialect.name());
        function_value(&received[0]["fallback"]);
        let (raised, received) = slots_raised(&dialect, null).await;
        assert_eq!(raised, "none", "{}", dialect.name());
        assert_eq!(received[0]["fallback"], serde_json::Value::Null);
    }
}

/// X-LASH-ONE on open objects and local references (FIG-6966): a
/// pydantic-shaped open object's `Optional[Callable]` field and its field
/// of a marked `$defs` type are presented callable in either dialect and
/// take a function, which crosses as a function value; its declared text
/// and its undeclared rest stay data, and a function there raises
/// `not_data` naming its path. Main typed the whole open object `Json`, so
/// the function raised `not_data` in every slot.
#[tokio::test(flavor = "multi_thread")]
async fn a_function_crosses_in_a_marked_slot_of_an_open_object_and_behind_a_local_reference() {
    for (dialect, crossing, extra, callable) in [
        (
            CellDialect::typescript(),
            "{ label: 'x', callback: f, handler: f, extra: { n: 1 } }",
            "{ label: 'x', extra: { g: f } }",
            "=> Promise<unknown>",
        ),
        (
            CellDialect::python(),
            "{'label': 'x', 'callback': f, 'handler': f, 'extra': {'n': 1}}",
            "{'label': 'x', 'extra': {'g': f}}",
            "Callable[..., Any]",
        ),
    ] {
        let signature =
            crate::loaded_tool_signature(&dialect, &serde_json::json!({}), &open_take())
                .expect("an addressable tool");
        assert!(
            signature.contains("handler") && signature.matches(callable).count() >= 2,
            "{}: both marked slots are presented callable: {signature}",
            dialect.name()
        );
        let (raised, received) = take_raised(open_take, "model", &dialect, crossing).await;
        assert_eq!(raised, "none", "{}", dialect.name());
        for slot in ["callback", "handler"] {
            function_value(&received[0][slot]);
        }
        assert_eq!(received[0]["extra"], serde_json::json!({"n": 1}));
        let (raised, received) = take_raised(open_take, "model", &dialect, extra).await;
        assert!(
            raised.contains("a function cannot leave the run")
                && raised.contains("`input.extra.g`"),
            "{}: {raised}",
            dialect.name()
        );
        assert!(
            received.is_empty(),
            "{}: nothing was requested",
            dialect.name()
        );
    }
}

/// CROSS-PATH: a function in a slot a JSON schema types as data (a text, a
/// closed object, an open object and everything in it) raises `not_data`
/// naming its argument path at the `perform`, and the tool is not called.
#[tokio::test(flavor = "multi_thread")]
async fn a_function_in_a_json_schema_data_slot_raises_not_data_naming_its_path() {
    let cases = [
        (
            CellDialect::typescript(),
            "{ definition: f, args: { g: f } }",
            "input.args.g",
        ),
        (
            CellDialect::python(),
            "{'definition': f, 'args': {'g': f}}",
            "input.args.g",
        ),
        (CellDialect::typescript(), "{ label: f }", "input.label"),
        (CellDialect::typescript(), "{ plan: f }", "input.plan"),
        (
            CellDialect::typescript(),
            "{ plan: { step: f } }",
            "input.plan.step",
        ),
    ];
    for (dialect, input, path) in cases {
        let (raised, received) = slots_raised(&dialect, input).await;
        assert!(
            raised.contains("a function cannot leave the run")
                && raised.contains(&format!("`{path}`")),
            "{}: {input}: {raised}",
            dialect.name()
        );
        assert!(received.is_empty(), "{input}: nothing was requested");
    }
}

/// FN-ID: a function value's identity is its rooted code and its frozen
/// captures. Equal code bound to different names, sent twice, and the same
/// code written again in a later cell are one document; a changed capture
/// is another. A parameter's name is part of the start signature, and what
/// the dialect knew of a capture's type shapes the code it wrote, so both
/// are part of the code; the functions here read their capture alike in
/// either cell.
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
    let resumed = crash_at_the_held_call_and_resume(&mut host, &tools, &services, code).await;
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

/// FIG-6892 / K-CAP-003: captions travel with the function, including when
/// it is saved and sent from a later cell. Captures prepend code and crossing
/// canonicalizes names; the two titles must still name their own statements.
async fn captioned_function(dialect: CellDialect, source: &str, send: &str) {
    let (received, host) = kept_with_host(&dialect, &[source, send]).await;
    for args in received {
        let value = function_value(&args["run"]);
        let captions = &value.captions;
        let encoded = lash_vocabulary::tokens::encode_body(&value);
        let decoded: FunctionValue =
            lash_vocabulary::tokens::decode_body(&encoded).expect("stored function value");
        assert_eq!(decoded, value, "captions survive the stored token boundary");
        let context = host.context();
        let backend = context.backend();
        let linked = backend
            .package_store()
            .link(&value.callable.link)
            .await
            .unwrap()
            .unwrap();
        let bytes = backend
            .module_artifacts()
            .get_module_artifact(&linked.link.document.to_string())
            .await
            .unwrap()
            .unwrap();
        let document =
            lash_vocabulary::tokens::decode_body::<lash_kernel_doc::Document>(&bytes).unwrap();
        captions.validate(&document).expect("new document sites");
        let titles: Vec<_> = captions
            .entries()
            .iter()
            .map(|entry| {
                let site = entry.target.site().expect("statement caption");
                let node = document.node(site).expect("captioned statement");
                (entry.caption.title(), format!("{node:?}"))
            })
            .collect();
        assert_eq!(titles.len(), 2, "only the extracted function's statements");
        assert!(
            titles
                .iter()
                .any(|(title, node)| *title == "Remember input" && node.contains("Let")),
            "{titles:?}"
        );
        assert!(
            titles
                .iter()
                .any(|(title, node)| *title == "Return result" && node.contains("Return")),
            "{titles:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_typescript_cell_function_keeps_its_statement_captions_at_crossing() {
    captioned_function(CellDialect::typescript(),
        "const offset = 3;\nconst run = async (x: number) => {\n/** Remember input */\nconst result = x + offset;\n/** Return result */\nreturn result;\n};\nawait keep.run({ run });",
        "await keep.run({ run });").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_python_cell_function_keeps_its_statement_captions_at_crossing() {
    captioned_function(CellDialect::python(),
        "offset = 3\nasync def run(x: int):\n    #: Remember input\n    result = x + offset\n    #: Return result\n    return result\nawait keep.run({'run': run})",
        "await keep.run({'run': run})").await;
}

/// L16/TR24: forwarding captures only the builtin path, declares the start
/// contract, and remains a function for Json rejection (K-EFF-002).
#[tokio::test(flavor = "multi_thread")]
async fn callable_wrapper_crossing_preserves_its_declared_contract() {
    let dialect = CellDialect::typescript();
    for (module, input) in [
        ("slots", "{ definition: Math.abs }"),
        ("slots", "{ definition: Reflect.get(Math, 'abs') }"),
        (
            "({ take: async input => await slots.take(...[input]) })",
            "{ definition: Math.abs }",
        ),
    ] {
        let (raised, received) = take_raised(slots, module, &dialect, input).await;
        assert_eq!(raised, "none", "{input}: {raised}");
        let function = function_value(&received[0]["definition"]);
        assert_eq!(function.signature().params.len(), 1);
        assert_eq!(function.signature().params[0].name.as_str(), "arg0");
        assert!(function.signature().params[0].optional);
        assert_eq!(function.captures.len(), 1);
        assert!(function.captures.values().all(|value| matches!(value,
            lash_kernel_doc::CaptureValue::Data(lash_kernel_doc::Datum::Text(path)) if path == "Math.abs")));
    }
    let (raised, received) = slots_raised(&dialect, "{ definition: Math.abs.bind(null) }").await;
    assert_eq!(
        raised,
        "a function cannot leave the run: the function states no start signature"
    );
    assert!(received.is_empty());
    let (raised, received) = slots_raised(&dialect, "{ args: { function: Math.abs } }").await;
    assert!(
        raised.contains("a function cannot leave the run")
            && raised.contains("input.args.function"),
        "{raised}"
    );
    assert!(received.is_empty());
}
