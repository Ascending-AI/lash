//! The standard protocol's prompt sections (ADR 0133, FIG-5257): what each
//! renders over the call's offered tools, and how a host's sections and
//! trusted wrappers complement or replace them.

use super::*;
use lash_core::plugin::prompt::{OfferedTools, PromptCall, PromptCatalog};
use lash_core::prompt_sections::{PromptPlan, PromptPurpose};
use lash_core::testing::prompt::{ComposedPrompt, PromptCutParts};

fn standard(config: StandardProtocolConfig) -> Arc<dyn SessionPlugin> {
    Arc::new(StandardProtocolPlugin {
        config,
        termination: lash_core::TerminationMode::Natural,
    })
}

async fn compose(
    plugins: &[Arc<dyn SessionPlugin>],
    purpose: PromptPurpose,
    catalog: lash_core::ToolCatalog,
) -> ComposedPrompt {
    let cut = lash_core::testing::prompt::cut(PromptCutParts {
        call: PromptCall {
            session_id: lash_core::SessionId::from("standard-prompt"),
            frame: None,
            run: None,
            turn: None,
            iteration: 0,
            call: 0,
            purpose: purpose.clone(),
        },
        config: Default::default(),
        session: None,
        offered: OfferedTools::new(Arc::new(catalog), false),
        model: Default::default(),
        history: Default::default(),
        namespaces: Default::default(),
    });
    lash_core::testing::prompt::compose(
        &PromptCatalog::of_plugins(plugins).expect("the plugins register"),
        &PromptPlan::default(),
        &purpose,
        cut,
    )
    .await
    .expect("the prompt composes")
}

fn instructions(composed: &ComposedPrompt) -> &str {
    composed.initial_instructions.as_deref().unwrap_or("")
}

fn tool(name: &str, module: &str, inline: bool) -> lash_core::ToolDefinition {
    let mut tool = lash_core::ToolDefinition::raw(
        name,
        name,
        "Issue operation",
        serde_json::json!({"type":"object"}),
        serde_json::json!({"type":"string"}),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120));
    tool.manifest.inline = inline;
    tool.manifest.module = Some(Arc::new(lash_core::ToolModule {
        name: module.into(),
    }));
    tool
}

fn module_tools() -> Vec<lash_core::ToolDefinition> {
    vec![
        tool("issue_search", "issues", true),
        tool("issue_read", "issues", true),
        tool("hidden", "deferred", false),
    ]
}

fn recorded(tools: Vec<lash_core::ToolDefinition>) -> lash_core::ToolCatalog {
    let catalog = lash_core::ToolCatalog::from_tool_definitions(tools);
    serde_json::from_value(serde_json::to_value(catalog).expect("record catalog"))
        .expect("read recorded catalog")
}

fn module_catalog() -> lash_core::ToolCatalog {
    recorded(module_tools())
}

/// The protocol contributes execution mechanics to initial instructions.
#[tokio::test]
async fn the_standard_sections_compose_the_protocol_prompt() {
    let composed = compose(
        &[standard(StandardProtocolConfig::default())],
        PromptPurpose::Turn,
        module_catalog(),
    )
    .await;
    assert_eq!(composed.current_context, None);
    insta::assert_snapshot!(instructions(&composed), @r"
    ## Execution

    Call tools directly with their declared JSON arguments. Use `batch` for two or more independent calls (at most 64 per batch); make dependent calls after their inputs return. Check each batch result’s success flag before using its value. Answer in prose only when no tool is needed.

    ");
}

/// Host interaction policy never follows a tool's name (FIG-5432).
#[tokio::test]
async fn an_ask_tool_does_not_select_interaction_policy() {
    let plugins = [standard(StandardProtocolConfig::default())];
    let empty = compose(
        &plugins,
        PromptPurpose::Turn,
        lash_core::ToolCatalog::default(),
    )
    .await;
    for inline in [true, false] {
        let asking = compose(
            &plugins,
            PromptPurpose::Turn,
            recorded(vec![tool("ask", "user", inline)]),
        )
        .await;
        assert_eq!(asking.initial_instructions, empty.initial_instructions);
        assert_eq!(
            instructions(&asking),
            format!(
                "## Execution\n\n{}",
                standard_execution_section(
                    BatchSugar::default(),
                    lash_core::TerminationMode::Natural
                )
            )
        );
    }
}
