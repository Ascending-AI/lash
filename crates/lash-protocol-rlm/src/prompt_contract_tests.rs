// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use crate::dialect::SessionDialect;

use lash_vm_runtime::{LashVmSurface, ToolBinding, ToolDefinitionBindingExt};

fn catalog() -> lash_core::ToolCatalog {
    lash_core::ToolCatalog::from_tool_definitions(catalog_definitions())
}

fn catalog_definitions() -> Vec<lash_core::ToolDefinition> {
    ((0..7).map(|index| {
            lash_core::ToolDefinition::raw(format!("tool:probe{index}"), format!("probe{index}"),
                "Return a STRING containing record-looking text, not a structured record.",
                serde_json::json!({"type":"object","properties":{"id":{"type":"string","description":"Record identifier"}},"required":["id"]}),
                serde_json::json!({"type":"string"})).expect("valid declared tool schemas").with_execution(std::time::Duration::from_secs(120))
                .with_tool_binding(ToolBinding::new(["probe"], format!("op{index}")))
        })).collect()
}

/// A catalogue carrying the process control surface.
///
/// FIG-2999: the process authoring block is gated by catalogue presence, not
/// by an ability, so a fixture that wants it declares a `processes.*` tool.
fn process_catalog() -> lash_core::ToolCatalog {
    let mut tools = catalog_definitions();
    tools.push(
        lash_core::ToolDefinition::raw(
            "tool:process-controls/start",
            "processes_start",
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
        .with_execution(std::time::Duration::from_secs(120))
        .with_tool_binding(ToolBinding::new(["processes"], "start")),
    );
    lash_core::ToolCatalog::from_tool_definitions(tools)
}

fn dialect(enabled: bool) -> SessionDialect {
    let surface = LashVmSurface {
        language_features: if enabled {
            lash_vm::LashVmLanguageFeatures::default().with_label_annotations()
        } else {
            lash_vm::LashVmLanguageFeatures::default()
        },
        ..LashVmSurface::default()
    };
    crate::dialect::SessionDialect::prompt_only(
        std::sync::Arc::new(crate::dialect::TypescriptDialect),
        surface,
    )
}

fn system_with(
    dialect: &SessionDialect,
    native: bool,
    enabled: bool,
    catalog: lash_core::ToolCatalog,
) -> String {
    let features = crate::protocol::RlmPromptFeatures {
        images: enabled,
        decomposition: enabled,
    };
    crate::prompt_sections::testing::compose_rlm(
        crate::prompt_sections::testing::RlmSections {
            dialect: std::sync::Arc::new(dialect.clone()),
            channel: if native {
                crate::plugin::RlmChannel::NativeTool
            } else {
                crate::plugin::RlmChannel::Cell
            },
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
        let dialect = dialect(enabled);
        for catalog in [catalog(), process_catalog()] {
            let prompt = system_with(&dialect, true, enabled, catalog);
            assert!(prompt.contains("### Tool transport"), "{prompt}");
            assert!(prompt.contains("`execute_code`"), "{prompt}");
            for needle in [
                "<typescript>",
                "</typescript>",
                "### Response shape",
                "### Example cell",
            ] {
                assert!(
                    !prompt.contains(needle),
                    "native prompt leaks cell syntax `{needle}`: {prompt}"
                );
            }
        }
    }
}

/// D-SLEEPALWAYS: every cell and native program can park on a durable timer,
/// independently of optional prompt features and process catalog membership.
#[test]
fn durable_sleep_is_taught_in_every_execution_channel() {
    let dialect = dialect(false);
    for native in [false, true] {
        for catalog in [catalog(), process_catalog()] {
            let prompt = system_with(&dialect, native, false, catalog);
            assert!(prompt.contains("await sleep(ms)"), "{prompt}");
        }
    }
}
