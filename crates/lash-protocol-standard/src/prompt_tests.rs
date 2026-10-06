use super::*;
use lash_core::plugin::ConfigRegistry;

fn registry() -> ConfigRegistry {
    ConfigRegistry::build(&[Arc::new(StandardProtocolPluginFactory::new())])
        .expect("standard config registry")
}

fn creation(
    prompt: Option<Value>,
    parent: Option<&lash_core::PluginConfig>,
) -> lash_core::PluginConfig {
    let mut options = lash_core::PluginOptions::default();
    if let Some(prompt) = prompt {
        options.insert_versioned(
            STANDARD_PROTOCOL_PLUGIN_ID,
            lash_core::FormatVersion::ONE,
            serde_json::json!({"prompt": prompt}),
        );
    }
    registry()
        .resolve_creation(
            Some(STANDARD_PROTOCOL_PLUGIN_ID),
            &options,
            parent,
            parent.is_none(),
            &lash_core::store::plugin_writers::PluginAdmission::default(),
        )
        .expect("standard prompt creation is accepted")
}

fn configured_prompt() -> Value {
    serde_json::json!({
        "intro": "You help maintain this project.",
        "instructions": ["Use the project's conventions."],
        "context": ["Working directory: /project"],
        "omit_builtin_guidance": false
    })
}

#[test]
fn creation_override_is_recorded_and_children_inherit_it() {
    let parent = creation(Some(configured_prompt()), None);
    assert_eq!(
        parent
            .get(STANDARD_PROTOCOL_PLUGIN_ID)
            .expect("parent namespace")["prompt"],
        configured_prompt()
    );
    let child = creation(None, Some(&parent));
    assert_eq!(
        child
            .get(STANDARD_PROTOCOL_PLUGIN_ID)
            .expect("child namespace")["prompt"],
        configured_prompt()
    );
    let explicit_child = creation(
        Some(serde_json::json!({"intro":"other defaults"})),
        Some(&parent),
    );
    assert_eq!(
        explicit_child
            .get(STANDARD_PROTOCOL_PLUGIN_ID)
            .expect("child namespace")["prompt"],
        configured_prompt()
    );
    let recorded = parent
        .decode::<StandardRecordedConfig>(STANDARD_PROTOCOL_PLUGIN_ID)
        .expect("decode parent")
        .expect("parent config");
    insta::assert_snapshot!(recorded.render_system_prompt(&module_catalog()), @r"
    You help maintain this project.

    ## Execution

    Call tools directly with their declared JSON arguments. Use `batch` for two or more independent calls (at most 64 per batch); make dependent calls after their inputs return. Check each batch result’s success flag before using its value. Answer in prose only when no tool is needed.

    ## Guidance

    - Be concise; no filler, hedging, or performative tone.
    - Act as soon as the next step is clear; do not restate conclusions.
    - Prefer the simplest correct solution.

    Use the project's conventions.

    ## Tool modules

    #### issues

    Search before reading an issue. Use cursors for pagination.

    #### deferred

    Deferred module instructions.

    ## Context

    Working directory: /project
    ");
}

#[test]
fn omitting_builtin_guidance_keeps_tool_modules_and_context() {
    let mut prompt = configured_prompt();
    prompt["omit_builtin_guidance"] = Value::Bool(true);
    let config = creation(Some(prompt), None);
    assert_eq!(
        config
            .get(STANDARD_PROTOCOL_PLUGIN_ID)
            .expect("recorded namespace")["prompt"]["omit_builtin_guidance"],
        true
    );
    let mut recorded = config
        .decode::<StandardRecordedConfig>(STANDARD_PROTOCOL_PLUGIN_ID)
        .expect("decode config")
        .expect("standard config");
    let text = recorded.render_system_prompt(&module_catalog());
    assert!(!text.contains("## Execution"));
    assert!(!text.contains("Be concise"));
    assert!(text.contains("Use the project's conventions."));
    assert_eq!(text.matches("Search before reading an issue.").count(), 1);
    assert!(text.ends_with("Working directory: /project"));
    recorded.prompt.intro = Some("".into());
    assert!(
        !recorded
            .render_system_prompt(&module_catalog())
            .contains("You are an assistant")
    );
    recorded.behaviour.discovery_operation = Some("issue_search".into());
    let text = recorded.render_system_prompt(&module_catalog());
    assert_eq!(text.matches("Search before reading an issue.").count(), 1);
    assert!(!text.contains("Deferred module instructions."));
    assert!(text.ends_with("Working directory: /project"));
}

fn module_catalog() -> lash_core::ToolCatalog {
    let tools = ["issue_search", "issue_read", "hidden"].map(|name| {
        let mut tool = lash_core::ToolDefinition::raw(
            name,
            name,
            "Issue operation",
            serde_json::json!({"type":"object"}),
            serde_json::json!({"type":"string"}),
        )
        .expect("valid declared tool schemas");
        let hidden = name == "hidden";
        tool.manifest.inline = !hidden;
        tool.manifest.module = Some(Arc::new(lash_core::ToolModule {
            name: if hidden { "deferred" } else { "issues" }.into(),
            instructions: Some(
                if hidden {
                    "Deferred module instructions."
                } else {
                    "Search before reading an issue. Use cursors for pagination."
                }
                .into(),
            ),
        }));
        tool
    });
    let catalog = lash_core::ToolCatalog::from_tool_definitions(Vec::from(tools));
    serde_json::from_value(serde_json::to_value(catalog).expect("record catalog"))
        .expect("read recorded catalog")
}

/// Compaction retains the recorded host prompt and behavioural guidance,
/// while omitting execution guidance for the summarizer's tool-free request.
#[test]
fn compaction_prompt_retains_host_sections_without_execution() {
    let recorded = creation(Some(configured_prompt()), None)
        .decode::<StandardRecordedConfig>(STANDARD_PROTOCOL_PLUGIN_ID)
        .expect("decode recorded prompt")
        .expect("standard namespace");
    insta::assert_snapshot!(recorded.render_compaction_prompt(), @r"
    You help maintain this project.

    ## Guidance

    - Be concise; no filler, hedging, or performative tone.
    - Act as soon as the next step is clear; do not restate conclusions.
    - Prefer the simplest correct solution.

    Use the project's conventions.

    ## Context

    Working directory: /project
    ");
    let mut without_guidance = recorded;
    without_guidance.prompt.omit_builtin_guidance = true;
    assert_eq!(
        without_guidance.render_compaction_prompt(),
        "You help maintain this project.\n\n## Guidance\n\nUse the project's conventions.\n\n## Context\n\nWorking directory: /project"
    );
}
