// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use crate::dialect::SessionDialect;

use lash_lashlang_runtime::{LashlangSurface, ToolBinding, ToolDefinitionBindingExt};

fn catalog() -> lash_core::ToolCatalog {
    lash_core::ToolCatalog::from_tool_definitions(catalog_definitions())
}

fn catalog_definitions() -> Vec<lash_core::ToolDefinition> {
    ((0..7).map(|index| {
            lash_core::ToolDefinition::raw(format!("tool:probe{index}"), format!("probe{index}"),
                "Return a STRING containing record-looking text, not a structured record.",
                serde_json::json!({"type":"object","properties":{"id":{"type":"string","description":"Record identifier"}},"required":["id"]}),
                serde_json::json!({"type":"string"})).expect("valid declared tool schemas")
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
        .with_tool_binding(ToolBinding::new(["processes"], "start")),
    );
    lash_core::ToolCatalog::from_tool_definitions(tools)
}

fn dialect(enabled: bool) -> SessionDialect {
    let surface = LashlangSurface {
        abilities: if enabled {
            lashlang::LashlangAbilities::all()
        } else {
            lashlang::LashlangAbilities::default()
        },
        language_features: if enabled {
            lashlang::LashlangLanguageFeatures::default().with_label_annotations()
        } else {
            lashlang::LashlangLanguageFeatures::default()
        },
        ..LashlangSurface::default()
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
    crate::system_prompt::render_system_prompt(
        dialect,
        &crate::system_prompt::RlmSystemPromptBehaviour {
            channel: if native {
                crate::plugin::RlmChannel::NativeTool
            } else {
                crate::plugin::RlmChannel::Cell
            },
            prompt_features: features,
            discovery: None,
        },
        crate::system_prompt::RlmSystemPromptInput {
            prompt: &lash_rlm_types::RlmPrompt::default(),
            tool_catalog: &catalog,
            bindings: &crate::projection::RlmProjectedBindings::new(),
            subagent: None,
        },
        crate::system_prompt::RlmSystemPromptScope::Turn,
    )
}

#[test]
fn typescript_capabilities_gate_in_both_assembled_channels() {
    for native in [false, true] {
        for sleep in [false, true] {
            for process_surface in [false, true] {
                let dialect = crate::dialect::SessionDialect::prompt_only(
                    std::sync::Arc::new(crate::dialect::TypescriptDialect),
                    LashlangSurface {
                        abilities: lashlang::LashlangAbilities { sleep },
                        ..Default::default()
                    },
                );
                let catalog = if process_surface {
                    process_catalog()
                } else {
                    catalog()
                };
                let prompt = system_with(&dialect, native, false, catalog);
                for (needle, enabled) in [
                    ("### Processes", process_surface),
                    ("Captures are by value", process_surface),
                    ("waitSignal", process_surface),
                    ("await sleep(ms)", sleep),
                ] {
                    assert_eq!(
                        prompt.contains(needle),
                        enabled,
                        "sleep={sleep}, process_surface={process_surface}, native={native}, {needle}"
                    );
                }
                // These retired surface syntaxes must never enter TypeScript
                // copy, and neither may the deleted special forms (FIG-2999).
                for needle in [
                    "@label",
                    "Type {",
                    "### Type literals",
                    "sleep for",
                    "wait_signal",
                    "defineProcess",
                    "registerTrigger",
                ] {
                    assert!(!prompt.contains(needle), "{needle}: {prompt}");
                }
            }
        }
    }
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
