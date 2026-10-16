//! FIG-6294: a tool's input may be omitted exactly when its canonical input
//! schema accepts the empty object, in TypeScript and Python cells alike.

use super::*;

/// A `forms.<operation>` tool of `input` schema that answers its input.
fn form(operation: &str, input: serde_json::Value) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:forms_{operation}"),
        format!("forms_{operation}"),
        "Answers the input it was called with.",
        input,
        serde_json::json!({ "type": "object" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_vm_runtime::ToolBinding::new(["forms"], operation))
}

/// The tool every field of whose input is optional, one with a default an
/// omitted input is not given.
const OPTIONAL: &str = "optional";

/// Schemas that do not accept `{}`, though none of them, or not all, says so
/// with a top-level `required`.
const REFUSING: [&str; 7] = [
    "required",
    "nonempty",
    "composed",
    "chosen",
    "referenced",
    "negated",
    "text",
];

fn forms() -> Vec<lash_core::ToolDefinition> {
    vec![
        form(
            OPTIONAL,
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": { "q": { "type": "string", "default": "all" } },
            }),
        ),
        form(
            "required",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["q"],
                "properties": { "q": { "type": "string" } },
            }),
        ),
        form(
            "nonempty",
            serde_json::json!({ "type": "object", "minProperties": 1 }),
        ),
        form(
            "composed",
            serde_json::json!({ "allOf": [{ "type": "object" }, { "required": ["q"] }] }),
        ),
        form(
            "chosen",
            serde_json::json!({ "oneOf": [
                { "type": "object", "required": ["a"] },
                { "type": "object", "required": ["b"] },
            ] }),
        ),
        form(
            "referenced",
            serde_json::json!({
                "$ref": "#/definitions/Query",
                "definitions": { "Query": { "type": "object", "required": ["q"] } },
            }),
        ),
        form(
            "negated",
            serde_json::json!({ "not": { "type": "object", "maxProperties": 0 } }),
        ),
        form("text", serde_json::json!({ "type": "string" })),
    ]
}

/// The forms, recording each input they execute with.
#[derive(Default)]
struct Forms {
    executed: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
}

impl Forms {
    fn executed(&self) -> Vec<(String, serde_json::Value)> {
        self.executed.lock().expect("executed calls").clone()
    }

    fn definition(key: &str) -> Option<lash_core::ToolDefinition> {
        forms()
            .into_iter()
            .find(|tool| tool.manifest.name == key || tool.manifest.id.as_str() == key)
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Forms {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash_core::PluginError> {
        Ok(forms()
            .iter()
            .map(lash_core::ToolDefinition::manifest)
            .collect())
    }

    fn resolve_manifest_by_id(
        &self,
        id: &lash_core::ToolId,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<lash_core::ToolManifest> {
        Self::definition(id.as_str()).map(|tool| tool.manifest())
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        Self::definition(name).map(|tool| Arc::new(tool.contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executed
            .lock()
            .expect("executed calls")
            .push((call.name().to_owned(), call.args.clone()));
        lash_core::ToolAttemptOutcome::done_without_intents(lash_core::ToolOutcomeDone::ok(
            serde_json::json!({ "seen": call.args }),
        ))
    }
}

/// How a cell of one dialect is written.
struct Spelling {
    dialect: CellDialect,
    workers: fn() -> lash_vm_client::service::Service,
    call: fn(&str) -> String,
    finish: &'static str,
    null: &'static str,
    /// Today's refusal of a call that leaves out a required input.
    missing: fn(&str) -> String,
    /// The refusal of a call with two inputs.
    excess: &'static str,
}

fn spellings() -> [Spelling; 2] {
    [
        Spelling {
            dialect: CellDialect::typescript(),
            workers: lash_vm_client::service::Service::default,
            call: |operation| format!("forms.{operation}"),
            finish: "control.finish",
            null: "null",
            missing: |operation| {
                format!("`forms.{operation}` takes 1 argument(s) and is called with 0")
            },
            excess: "takes 0 to 1 argument(s) and is called with 2",
        },
        Spelling {
            dialect: CellDialect::python(),
            workers: python_workers,
            call: |operation| format!("forms_{operation}"),
            finish: "control_finish",
            null: "None",
            missing: |operation| {
                format!("the call gives the tool `forms_{operation}` no value for `input`")
            },
            excess: "takes 1 arguments but 2 were given",
        },
    ]
}

/// A distinct replay key for each cell of the law.
fn replay_key(index: usize) -> &'static str {
    Box::leak(format!("exec-code:{index}").into_boxed_str())
}

/// TOOL-INPUT-OMISSION: an all-optional tool called with nothing executes
/// with exactly `{}`, as when called with `{}`, and its schema's default is
/// not applied. A schema that refuses `{}` (a required field, `minProperties`,
/// `allOf`, `oneOf`, a `$ref`, `not`, a non-object) still refuses the call
/// that leaves its input out, with today's error, and runs nothing. An
/// explicit null is the value it is, not an omission, and more than one
/// input is still refused. The prompt states the call convention: an
/// omittable input shows `{}` as its default.
#[tokio::test(flavor = "multi_thread")]
async fn tool_input_omission_follows_canonical_schema() {
    let host = open_host().await;
    let mut cells = 0;
    for spelling in spellings() {
        let name = spelling.dialect.name().to_owned();
        let services = cell_services(&spelling.dialect, (spelling.workers)());
        let mut run = |code: String, tools: Arc<Forms>| {
            cells += 1;
            let key = replay_key(cells);
            let services = &services;
            let dialect = &spelling.dialect;
            let host = &host;
            async move {
                let mut state = crate::testing::session_state(dialect);
                run_cell(
                    &mut state,
                    cell_context(host, SESSION, TURN, key, tools),
                    services,
                    &code,
                )
                .await
            }
        };
        let optional = (spelling.call)(OPTIONAL);

        let tools = Arc::new(Forms::default());
        let response = run(
            format!(
                "await {}([await {optional}(), await {optional}({{}})])",
                spelling.finish
            ),
            tools.clone(),
        )
        .await;
        assert_eq!(
            finish_of(&response),
            serde_json::json!([{ "seen": {} }, { "seen": {} }]),
            "{name}"
        );
        assert_eq!(
            tools.executed(),
            vec![("forms_optional".to_owned(), serde_json::json!({})); 2],
            "{name}: an omitted input executes as exactly {{}}, defaults unapplied"
        );

        for operation in REFUSING {
            let tools = Arc::new(Forms::default());
            let response = run(
                format!("await {}()", (spelling.call)(operation)),
                tools.clone(),
            )
            .await;
            let error = response
                .error()
                .unwrap_or_else(|| panic!("{name}: `{operation}` refuses an omitted input"));
            assert!(
                error.message.contains(&(spelling.missing)(operation)),
                "{name}: {error:?}"
            );
            assert!(
                tools.executed().is_empty(),
                "{name}: `{operation}` never ran"
            );
        }

        let tools = Arc::new(Forms::default());
        let response = run(
            format!("await {optional}({})", spelling.null),
            tools.clone(),
        )
        .await;
        assert!(
            response.error().is_some(),
            "{name}: an explicit null is validated as null and refused"
        );
        assert!(
            tools.executed().is_empty(),
            "{name}: the null call never ran"
        );

        let tools = Arc::new(Forms::default());
        let response = run(format!("await {optional}({{}}, {{}})"), tools.clone()).await;
        let error = response.error().expect("two inputs are refused");
        assert!(error.message.contains(spelling.excess), "{name}: {error:?}");
        assert!(
            tools.executed().is_empty(),
            "{name}: the excess call never ran"
        );

        let docs = crate::tool_catalog::rlm_prompt_tool_docs(
            &lash_core::ToolCatalog::from_tool_definitions(forms()),
            &crate::dialect::SessionDialect::prompt_only(spelling.dialect.clone()),
            Default::default(),
        );
        let signature = |operation: &str| {
            let call = (spelling.call)(operation);
            docs.lines()
                .find(|line| line.contains(&format!("{call}(")))
                .unwrap_or_else(|| panic!("{name}: `{call}` is listed: {docs}"))
                .to_owned()
        };
        assert!(
            signature(OPTIONAL).contains(" = {})"),
            "{name}: {}",
            signature(OPTIONAL)
        );
        for operation in REFUSING {
            assert!(
                !signature(operation).contains(" = {})"),
                "{name}: {}",
                signature(operation)
            );
        }
    }
}
