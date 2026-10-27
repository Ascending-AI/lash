// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use crate::dialect::SessionDialect;

fn catalog() -> lash_core::ToolCatalog {
    lash_core::ToolCatalog::from_tool_definitions(catalog_definitions())
}

fn catalog_definitions() -> Vec<lash_vocabulary::tool_contract::ToolDefinition> {
    ((0..7).map(|index| {
            lash_vocabulary::tool_contract::ToolDefinition::raw(format!("tool:probe{index}"), format!("probe.op{index}"),
                "Return a STRING containing record-looking text, not a structured record.",
                serde_json::json!({"type":"object","properties":{"id":{"type":"string","description":"Record identifier"}},"required":["id"]}),
                serde_json::json!({"type":"string"})).expect("valid declared tool schemas").with_execution(std::time::Duration::from_secs(120))
        })).collect()
}

/// A catalogue carrying the process control surface.
///
/// FIG-2999: the process authoring block is gated by catalogue presence, not
/// by an ability, so a fixture that wants it declares a `processes.*` tool.
fn process_catalog() -> lash_core::ToolCatalog {
    let mut tools = catalog_definitions();
    tools.push(
        lash_vocabulary::tool_contract::ToolDefinition::raw(
            "tool:process-controls/start",
            "processes.start",
            "Start a process",
            serde_json::json!({
                "type": "object",
                "properties": { "definition": { "type": "object" } },
                "required": ["definition"],
                "additionalProperties": false
            }),
            serde_json::json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false
            }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120)),
    );
    lash_core::ToolCatalog::from_tool_definitions(tools)
}

fn dialect() -> SessionDialect {
    crate::dialect::SessionDialect::prompt_only(crate::dialect::CellDialect::typescript())
}

fn system_with(dialect: &SessionDialect, enabled: bool, catalog: lash_core::ToolCatalog) -> String {
    let features = crate::protocol::RlmPromptFeatures {
        images: enabled,
        decomposition: enabled,
    };
    crate::prompt_sections::testing::compose_rlm(
        crate::prompt_sections::testing::RlmSections {
            dialect: std::sync::Arc::new(dialect.clone()),
            prompt_features: features,
            discovery: None,
            budget_tokens: None,
        },
        crate::prompt_sections::testing::Call {
            catalog,
            ..Default::default()
        },
    )
    .initial_instructions
    .unwrap_or_default()
}

#[test]
fn the_native_prompt_names_its_transport_and_carries_no_cell_syntax() {
    // FIG-2881: semantic guard for the authored native copy. The prompt must
    // teach the `execute_code` transport itself, and no cell-channel tag
    // syntax may survive into it — wording-only assertions let a derivation
    // silently produce incoherent copy.
    for enabled in [false, true] {
        let dialect = dialect();
        for catalog in [catalog(), process_catalog()] {
            let prompt = system_with(&dialect, enabled, catalog);
            assert!(prompt.contains("### Tool transport"), "{prompt}");
            assert!(prompt.contains("`execute_code`"), "{prompt}");
            for needle in ["### Response shape", "### Example cell"] {
                assert!(
                    !prompt.contains(needle),
                    "native prompt leaks cell syntax `{needle}`: {prompt}"
                );
            }
        }
    }

    // FIG-7468: a worked finish must accept the same value as the declared
    // tool. Review the transport and discovery rhythm as rendered text.
    let mut openings = String::new();
    for cell_dialect in [
        crate::dialect::CellDialect::typescript(),
        crate::dialect::CellDialect::python(),
    ] {
        let dialect = SessionDialect::prompt_only(cell_dialect);
        let spelling = dialect.language().spelling();
        for schema in [
            serde_json::json!({"type": "string"}),
            serde_json::json!({"type": "number"}),
            serde_json::json!({"type": "integer", "minimum": 7}),
            serde_json::json!({"type": "boolean"}),
            serde_json::json!({"type": "null"}),
            serde_json::json!({"type": "array", "items": {"type": "string"}, "minItems": 1}),
            serde_json::json!({"type": "string", "pattern": "^accepted$"}),
            serde_json::json!({"type": "object", "properties": {"answer": {"type": "string"}}, "required": ["answer"], "additionalProperties": false}),
            serde_json::json!({"enum": ["accepted", "declined"]}),
            serde_json::json!({"$defs": {"Answer": {"type": "boolean"}}, "$ref": "#/$defs/Answer"}),
        ] {
            let schema = lash_kernel_value::json_schema::JsonSchema::admit(schema)
                .expect("valid finish schema");
            let finish = crate::control_tools::finish_tool_definition(dialect.language())
                .with_finish_value_schema(schema.clone())
                .expect("the finish tool is typed");
            let prompt = dialect
                .render_execution_section(
                    Default::default(),
                    &lash_core::ToolCatalog::from_tool_definitions(vec![finish]),
                    None,
                )
                .expect("the finish tool presents");
            let opening = prompt.split("\n\nLexical").next().unwrap();
            if schema.as_value() == &serde_json::json!({"type": "string"}) {
                openings.push_str(&format!("{spelling:?}\n{opening}\n\n"));
            }
            let call = prompt
                .split("execute_code(")
                .nth(1)
                .expect("example call")
                .lines()
                .next()
                .unwrap()
                .strip_suffix(')')
                .unwrap();
            let args: serde_json::Value = serde_json::from_str(call).expect("native arguments");
            let code = args["code"].as_str().unwrap();
            if schema.as_value().get("pattern").is_some() {
                assert_eq!(
                    code,
                    match spelling {
                        lash_kernel_dialect::Spelling::TypeScript => "console.log(1 + 2);",
                        lash_kernel_dialect::Spelling::Python => "print(1 + 2)",
                    },
                    "an unsupported constraint must not teach an invalid finish"
                );
                continue;
            }
            let literal = code
                .strip_prefix("await control.finish(")
                .expect("the example finishes with a schema-derived literal")
                .trim_end_matches(';')
                .strip_suffix(')')
                .unwrap();
            let format = match spelling {
                lash_kernel_dialect::Spelling::TypeScript => {
                    lash_kernel_dialect::SourceFormat::TypeScriptV1
                }
                lash_kernel_dialect::Spelling::Python => {
                    lash_kernel_dialect::SourceFormat::PythonV1
                }
            };
            let value: serde_json::Value =
                serde_json::from_str(&lash_kernel_dialect::metadata_json(literal, format))
                    .expect("the example is a native literal");
            schema
                .validate(&value)
                .expect("the example matches the effective finish contract");
        }
    }
    insta::assert_snapshot!(openings, @r#"
TypeScript
Use prose for conversation; use the `execute_code` program for action or computation. Use only tools listed under **Tools**; copy their exact dotted paths and argument shapes, and await each call.

### Tool transport

Run one cell per `execute_code` call with `{"code": "<complete program>"}`. Call tools inside the program; prose before the call is commentary. Tool results and `console.log` output are read in the next step: look things up in one cell, inspect the output, then act or finish in a later cell.

### Example execute_code call

execute_code({"code":"await control.finish(\"Example answer\");"})

Python
Use prose for conversation; use the `execute_code` program for action or computation. Use only tools listed under **Tools**; copy their exact dotted paths and argument shapes, pass one dict of arguments, and await each call.

### Tool transport

Run one cell per `execute_code` call with `{"code": "<complete program>"}`. Call tools inside the program. Tool results and `print` output are read in the next step: look things up in one cell, inspect the output, then act or finish in a later cell.

### Example execute_code call

execute_code({"code":"await control.finish(\"Example answer\")"})
"#);
}

/// D-SLEEPALWAYS: every cell and native program can park on a durable timer,
/// independently of optional prompt features and process catalog membership.
#[test]
fn durable_sleep_is_taught_for_native_execution() {
    let dialect = dialect();
    {
        for catalog in [catalog(), process_catalog()] {
            let prompt = system_with(&dialect, false, catalog);
            assert!(prompt.contains("await sleep(ms)"), "{prompt}");
        }
    }
}
