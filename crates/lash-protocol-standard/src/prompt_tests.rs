//! The standard protocol's prompt sections (ADR 0133, FIG-5257): what each
//! renders over the call's offered tools, and how a host's sections and
//! trusted wrappers complement or replace them.

use super::*;
use lash_core::plugin::prompt::{
    OfferedTools, PromptCall, PromptCatalog, PromptInput, PromptSectionSpec, PromptWrapSpec,
    PromptWrapTarget, SectionText,
};
use lash_core::prompt_sections::{
    PromptPlacement, PromptPlan, PromptPurpose, PromptSectionId, PromptSectionKey, PromptWrapKey,
};
use lash_core::testing::prompt::{ComposedPrompt, PromptCutParts};

fn standard(config: StandardProtocolConfig) -> Arc<dyn SessionPlugin> {
    Arc::new(StandardProtocolPlugin { config })
}

fn key(key: &str) -> PromptSectionKey {
    PromptSectionKey::new(key).expect("valid section key")
}

fn standard_section(local: &str) -> PromptSectionId {
    PromptSectionId::new(STANDARD_PROTOCOL_PLUGIN_ID, key(local))
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

/// The protocol's sections compose in registration order, every one in the
/// initial instructions: intro, execution and guidance. A tool's guidance is
/// its own plugin's section, not the protocol's.
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
    You are an assistant operating the lash harness.

    ## Execution

    Call tools directly with their declared JSON arguments. Use `batch` for two or more independent calls (at most 64 per batch); make dependent calls after their inputs return. Check each batch result’s success flag before using its value. Answer in prose only when no tool is needed.

    ## Guidance

    - Be concise; no filler, hedging, or performative tone.
    - Act as soon as the next step is clear; do not restate conclusions.
    - Prefer the simplest correct solution.
    ");
}

/// OFFERED (FIG-5257): the tool sections describe exactly the call's offered
/// surface. The interactive bullet follows an `ask` the model sees: an
/// offered inline one, not one a discovery session keeps out of the inline
/// surface, and none in a call that offers no tools.
#[tokio::test]
async fn the_tool_sections_describe_exactly_the_offered_tools() {
    let mut with_ask = module_tools();
    with_ask.push(tool("ask", "user", true));
    let asking = compose(
        &[standard(StandardProtocolConfig::default())],
        PromptPurpose::Turn,
        recorded(with_ask),
    )
    .await;
    assert!(instructions(&asking).contains("Ask only when progress is blocked"));

    let mut deferred_ask = module_tools();
    deferred_ask.push(tool("ask", "user", false));
    let discovering = compose(
        &[standard(StandardProtocolConfig {
            discovery: Some(lash_core::ToolDiscovery {
                operation: "issue_search".into(),
            }),
            ..StandardProtocolConfig::default()
        })],
        PromptPurpose::Turn,
        recorded(deferred_ask),
    )
    .await;
    let text = instructions(&discovering);
    assert!(
        !text.contains("Ask only when progress is blocked"),
        "{text}"
    );

    let empty = compose(
        &[standard(StandardProtocolConfig::default())],
        PromptPurpose::Turn,
        lash_core::ToolCatalog::default(),
    )
    .await;
    assert!(!instructions(&empty).contains("Ask only when progress is blocked"));
}

/// A host adds its own text as sections of its own plugin, and replaces or
/// omits a built-in section by wrapping it: the protocol has no host prompt
/// config.
#[tokio::test]
async fn a_host_replaces_the_intro_omits_guidance_and_adds_context() {
    struct Host;
    impl SessionPlugin for Host {
        fn id(&self) -> &'static str {
            "host"
        }
        fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
            reg.prompt().wrap(
                PromptWrapSpec::new(
                    PromptWrapKey::new("intro").expect("valid wrap key"),
                    standard_section(section_keys::INTRO),
                ),
                Arc::new(
                    |_: &PromptInput<'_>, _: PromptWrapTarget<'_>, _: SectionText| {
                        Ok(SectionText::text("You help maintain this project."))
                    },
                ),
            )?;
            reg.prompt().wrap(
                PromptWrapSpec::new(
                    PromptWrapKey::new("guidance").expect("valid wrap key"),
                    standard_section(section_keys::GUIDANCE),
                ),
                Arc::new(
                    |_: &PromptInput<'_>, _: PromptWrapTarget<'_>, _: SectionText| {
                        Ok(SectionText::Omit)
                    },
                ),
            )?;
            reg.prompt().section(
                PromptSectionSpec::new(key("context"), PromptPlacement::InitialInstructions)
                    .purposes([PromptPurpose::Turn, PromptPurpose::Compaction]),
                Arc::new(|_: &PromptInput<'_>| {
                    Ok(SectionText::text(
                        "## Context\n\nWorking directory: /project",
                    ))
                }),
            )
        }
    }
    let plugins: [Arc<dyn SessionPlugin>; 2] =
        [standard(StandardProtocolConfig::default()), Arc::new(Host)];
    let turn = compose(&plugins, PromptPurpose::Turn, module_catalog()).await;
    let text = instructions(&turn);
    assert!(
        text.starts_with("You help maintain this project.\n\n## Execution"),
        "{text}"
    );
    assert!(!text.contains("Be concise"), "{text}");
    assert!(text.ends_with("Working directory: /project"), "{text}");

    // Compaction keeps the host's sections and the protocol's text that
    // declares the compaction purpose, and leaves out the tool sections.
    let compaction = compose(
        &plugins,
        PromptPurpose::Compaction,
        lash_core::ToolCatalog::default(),
    )
    .await;
    assert_eq!(
        instructions(&compaction),
        "You help maintain this project.\n\n## Context\n\nWorking directory: /project"
    );
}

/// Compaction's summarizer call ships no tools: the intro and the
/// behavioural guidance render, the execution and tool sections do not.
#[tokio::test]
async fn the_compaction_prompt_keeps_intro_and_guidance_without_execution() {
    let composed = compose(
        &[standard(StandardProtocolConfig::default())],
        PromptPurpose::Compaction,
        lash_core::ToolCatalog::default(),
    )
    .await;
    insta::assert_snapshot!(instructions(&composed), @r"
    You are an assistant operating the lash harness.

    ## Guidance

    - Be concise; no filler, hedging, or performative tone.
    - Act as soon as the next step is clear; do not restate conclusions.
    - Prefer the simplest correct solution.
    ");
}
