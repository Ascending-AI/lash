//! FIG-4588 laws of the code-mode system prompt: what the protocol renders
//! from a recorded prompt config, a recorded catalog, bindings and subagent
//! authority, and what each omission removes.

use std::sync::Arc;

use lash_lashlang_runtime::{LashlangSurface, ToolBinding, ToolDefinitionBindingExt};
use lash_rlm_types::{RlmPrompt, RlmPromptIntro};

use super::*;
use crate::RlmProjectorConfig;

fn tool(
    name: &str,
    module: &'static str,
    operation: &'static str,
    description: &str,
) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        description,
        serde_json::json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
        }),
        serde_json::json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
    .with_tool_binding(ToolBinding::new([module], operation))
}

/// Two tools of one MCP-style module that carries instructions, and one tool
/// of no module.
fn catalog() -> lash_core::ToolCatalog {
    let module = Arc::new(lash_core::ToolModule {
        name: "tracker".to_string(),
        instructions: Some("Search before you open an issue.".to_string()),
    });
    let mut search = tool("tracker_search", "tracker", "search", "Search issues.");
    search.manifest.module = Some(Arc::clone(&module));
    let mut open = tool("tracker_open", "tracker", "open", "Open an issue.");
    open.manifest.module = Some(module);
    lash_core::ToolCatalog::from_tool_definitions(vec![
        tool("grep", "files", "grep", "Search file contents."),
        search,
        open,
    ])
}

fn config(catalog: &lash_core::ToolCatalog) -> RlmProjectorConfig {
    RlmProjectorConfig {
        lashlang_surface: LashlangSurface::new(
            lashlang::LashlangAbilities::default(),
            lashlang::LashlangLanguageFeatures::default(),
            lashlang::LashlangHostCatalog::tool_default(
                catalog.tool_names().iter().map(String::as_str),
            ),
        ),
        ..RlmProjectorConfig::new(Arc::new(crate::dialect::TypescriptDialect))
    }
}

fn bindings() -> RlmProjectedBindings {
    RlmProjectedBindings::new()
        .bind_json("current_query", serde_json::json!("open issues"))
        .expect("bind")
}

fn subagent() -> lash_core::SubagentSessionContext {
    lash_core::SubagentSessionContext {
        capability: "research".to_string(),
        depth: 1,
    }
}

fn customised() -> RlmPrompt {
    RlmPrompt {
        intro: RlmPromptIntro::Host {
            text: "You are the release assistant of the tracker team.".to_string(),
        },
        omit_builtin_guidance: true,
        omit_builtin_execution: false,
        instructions: vec![
            "Answer in British English.".to_string(),
            "Never close an issue without a linked change.".to_string(),
        ],
        context: vec!["Release 4.2 freezes on Friday.".to_string()],
    }
}

/// The prompt over the fixture catalog with no bindings and no subagent.
fn render(prompt: &RlmPrompt) -> String {
    let catalog = catalog();
    render_rlm_system_prompt(
        &config(&catalog),
        RlmSystemPromptInput {
            prompt,
            tool_catalog: &catalog,
            bindings: &RlmProjectedBindings::new(),
            subagent: None,
        },
    )
}

/// The declarations the fixture catalog generates.
const DECLARATIONS: &str = r##"### Tools

`files.grep({ query: string }): Promise<string>`
Search file contents.

#### tracker

Search before you open an issue.

`tracker.search({ query: string }): Promise<string>`
Search issues.

`tracker.open({ query: string }): Promise<string>`
Open an issue."##;
/// The TypeScript dialect's built-in execution prose on the cell channel.
const EXECUTION_PROSE: &str = r##"Use prose for conversation; use a paired `<typescript>` block for action or computation. Call tools as `await module.operation({ ... })`, only those listed under **Tools**.

### Response shape

Put one program after any commentary, between standalone `<typescript>` and `</typescript>` lines. Markdown fences do not execute. A standalone `</typescript>` line ends the program even inside a multiline string; keep that line out of string contents.

### Example cell

<typescript>
const total = 1 + 2;
finish(total);
</typescript>

Top-level bindings persist across executions. Return exactly the value and type the task asks for with `finish(value)`; do not finish an unexamined whole tool result. Putting an object into a string — with `+`, `` `${...}` `` or `String(...)` — gives the placeholder `[object Object]`, never its contents; read the value with `console.log(value)` or serialize it with `JSON.stringify(value)`.

`Math`, `Date` (UTC), `String`, `Array`, `Object`, `JSON`, `Map`/`Set`, `RegExp` and `URL` are available; this is not Node or a browser, and classes and generators are not supported.

### Host API

`console.log(value)` shows output in the next step; `print(value)` shows a structured value, summarised field by field rather than cut off when it is large; `finish(value)` ends the turn. A failed tool call throws an `Error` whose `cause` is `{ code, details }`."##;
/// The declaration of the fixture binding.
const READ_ONLY_VARIABLES: &str = r##"### Read-Only Variables

These read-only values are already in scope. Access them directly in `<typescript>` cells; do not recreate them manually.

Read-only variables:
- `current_query`: `string`, read-only (descriptor: `string`)"##;
const BUILTIN_GUIDANCE_SECTION: &str = "## Guidance\n\n- Be concise; no filler, hedging, or performative tone.\n- Act as soon as the next step is clear; do not restate conclusions.\n- Prefer the simplest correct solution.";

/// A customised config's prompt, whole: the host's intro, its instructions
/// in place of the built-in guidance, the declarations with the session's
/// read-only variables and subagent description, and the context last.
#[test]
fn a_customised_prompt_renders_the_hosts_text_around_the_declarations() {
    let catalog = catalog();
    let rendered = render_rlm_system_prompt(
        &config(&catalog),
        RlmSystemPromptInput {
            prompt: &customised(),
            tool_catalog: &catalog,
            bindings: &bindings(),
            subagent: Some(&subagent()),
        },
    );
    assert_eq!(
        rendered,
        format!(
            "You are the release assistant of the tracker team.\n\n\
             ## Guidance\n\nAnswer in British English.\n\nNever close an issue without a linked change.\n\n\
             ## TypeScript execution\n\n{}\n\n{}\n\n{}\n\n\
             Subagent capability: research. Depth: 1/5.\n\n\
             ## Context\n\nRelease 4.2 freezes on Friday.",
            EXECUTION_PROSE, DECLARATIONS, READ_ONLY_VARIABLES,
        )
    );
}

/// Each built-in text is left out by its own switch and by nothing else, and
/// no switch removes a declaration.
#[test]
fn each_built_in_text_is_omitted_on_its_own() {
    let default = render(&RlmPrompt::default());
    let declarations = DECLARATIONS;
    let prose = EXECUTION_PROSE;
    assert!(default.contains(prose) && default.contains(declarations));

    let no_intro = render(&RlmPrompt {
        intro: RlmPromptIntro::Omitted,
        ..RlmPrompt::default()
    });
    assert_eq!(
        no_intro,
        default
            .strip_prefix(&format!("{RLM_BUILTIN_INTRO}\n\n"))
            .expect("the default opens with the built-in intro")
    );

    let no_guidance = render(&RlmPrompt {
        omit_builtin_guidance: true,
        ..RlmPrompt::default()
    });
    assert_eq!(
        no_guidance,
        default.replace(&format!("{BUILTIN_GUIDANCE_SECTION}\n\n"), "")
    );
    assert!(!no_guidance.contains("## Guidance"));

    let no_prose = render(&RlmPrompt {
        omit_builtin_execution: true,
        ..RlmPrompt::default()
    });
    assert_eq!(no_prose, default.replace(&format!("{prose}\n\n"), ""));
    assert!(no_prose.contains(declarations));

    let nothing_built_in = render(&RlmPrompt {
        intro: RlmPromptIntro::Omitted,
        omit_builtin_guidance: true,
        omit_builtin_execution: true,
        ..RlmPrompt::default()
    });
    assert_eq!(
        nothing_built_in,
        format!("## TypeScript execution\n\n{declarations}")
    );
}

/// A module's instructions render once, above its tools, however many tools
/// it has (FIG-4548).
#[test]
fn a_modules_instructions_render_once() {
    let rendered = render(&RlmPrompt::default());
    assert_eq!(
        rendered.matches("Search before you open an issue.").count(),
        1
    );
    let module = rendered.find("#### tracker").expect("module heading");
    for call in ["tracker.search(", "tracker.open("] {
        assert!(rendered.find(call).expect("declared tool") > module);
    }
}

/// A session with a discovery operation declares only its inline tools and
/// says how to find the rest, with or without the built-in prose.
#[test]
fn discovery_is_a_declaration() {
    let mut hidden = tool("hidden", "files", "hidden", "A tool found by search.");
    hidden.manifest.inline = false;
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![
        tool("search_tools", "tools", "search", "Find tools."),
        hidden,
    ]);
    let config = RlmProjectorConfig {
        discovery: Some(lash_core::ToolDiscovery {
            operation: "tools.search".to_string(),
        }),
        ..config(&catalog)
    };
    for omit_builtin_execution in [false, true] {
        let rendered = render_rlm_system_prompt(
            &config,
            RlmSystemPromptInput {
                prompt: &RlmPrompt {
                    omit_builtin_execution,
                    ..RlmPrompt::default()
                },
                tool_catalog: &catalog,
                bindings: &RlmProjectedBindings::new(),
                subagent: None,
            },
        );
        assert!(rendered.contains(
            "### Tools\n\nOther tools exist; find them with `await tools.search({ ... })`."
        ));
        assert!(rendered.contains("Find tools."));
        assert!(!rendered.contains("A tool found by search."));
    }
}

/// The native channel's prompt carries no cell tag line in either part, and
/// the same declarations.
#[test]
fn the_native_channel_renders_the_same_declarations_without_cells() {
    let catalog = catalog();
    let config = config(&catalog);
    let dialect =
        SessionDialect::prompt_only(Arc::clone(&config.dialect), config.lashlang_surface.clone());
    let rendered = render_system_prompt(
        &dialect,
        &RlmSystemPromptBehaviour {
            channel: RlmChannel::NativeTool,
            prompt_features: config.prompt_features,
            discovery: None,
        },
        RlmSystemPromptInput {
            prompt: &RlmPrompt::default(),
            tool_catalog: &catalog,
            bindings: &RlmProjectedBindings::new(),
            subagent: None,
        },
        RlmSystemPromptScope::Turn,
    );
    assert!(rendered.contains("### Tool transport"));
    assert!(rendered.contains(DECLARATIONS));
    assert!(
        rendered
            .lines()
            .all(|line| !["<typescript>", "</typescript>"].contains(&line.trim()))
    );
}

/// Blank host text renders nothing: no empty heading, no stray separator.
#[test]
fn blank_host_text_renders_no_section() {
    let rendered = render(&RlmPrompt {
        intro: RlmPromptIntro::Host {
            text: "  ".to_string(),
        },
        omit_builtin_guidance: true,
        instructions: vec![String::new()],
        context: vec!["\n".to_string()],
        ..RlmPrompt::default()
    });
    assert!(rendered.starts_with("## TypeScript execution\n\n"));
    assert!(!rendered.contains("## Guidance"));
    assert!(!rendered.contains("## Context"));
    assert!(!rendered.contains("\n\n\n"));
}
