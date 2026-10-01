use lash_core::facade_support::JsonSchema;
use lash_core::plugin::{ConfigCommand, OwnerChange};

use crate::{StandardConfigOwner, StandardRecordedConfig, standard_execution_section};

/// The standard protocol's host-authored prompt, recorded at creation and
/// changed only by its prompt commands. Empty text contributes no section.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(default, deny_unknown_fields)]
pub struct StandardPrompt {
    /// Replace the built-in intro. `Some("")` omits it.
    pub intro: Option<String>,
    pub instructions: Vec<String>,
    /// Context renders last, after tool modules.
    pub context: Vec<String>,
    /// Omit the built-in execution and behavioural guidance, retaining host
    /// instructions and tool module instructions.
    pub omit_builtin_guidance: bool,
}

/// Replace the session's entire standard prompt.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
pub struct SetStandardPrompt {
    pub prompt: StandardPrompt,
}

impl ConfigCommand for SetStandardPrompt {
    type Owner = StandardConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_prompt";
}

/// Replace only the session's standard prompt context.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
pub struct SetStandardPromptContext {
    pub context: Vec<String>,
}

impl ConfigCommand for SetStandardPromptContext {
    type Owner = StandardConfigOwner;
    type Output = ();
    const NAME: &'static str = "set_prompt_context";
}

pub(crate) fn replace_prompt(
    recorded: &StandardRecordedConfig,
    command: SetStandardPrompt,
) -> OwnerChange<StandardRecordedConfig, ()> {
    OwnerChange {
        recorded: StandardRecordedConfig {
            prompt: command.prompt,
            ..recorded.clone()
        },
        output: (),
    }
}

pub(crate) fn replace_context(
    recorded: &StandardRecordedConfig,
    command: SetStandardPromptContext,
) -> OwnerChange<StandardRecordedConfig, ()> {
    let mut recorded = recorded.clone();
    recorded.prompt.context = command.context;
    OwnerChange {
        recorded,
        output: (),
    }
}

const MAIN_AGENT_INTRO: &str = "You are an assistant operating the lash harness.";
const GUIDANCE_BASE: &[&str] = &[
    "- Be concise; no filler, hedging, or performative tone.",
    "- Act as soon as the next step is clear; do not restate conclusions.",
    "- Prefer the simplest correct solution.",
];
const GUIDANCE_INTERACTIVE: &str =
    "- Take initiative when the user's intent is clear. Ask only when progress is blocked.";

impl StandardRecordedConfig {
    /// Render from the admitted standard namespace and the recorded catalog.
    /// Replay serves the journaled result instead of invoking this renderer.
    pub fn render_system_prompt(&self, catalog: &lash_core::ToolCatalog) -> String {
        let visible_catalog;
        let catalog = if self.behaviour.discovery_operation.is_some() {
            visible_catalog = catalog.inline_tools();
            &visible_catalog
        } else {
            catalog
        };
        let mut sections = Vec::new();
        let intro = self
            .prompt
            .intro
            .as_deref()
            .unwrap_or(MAIN_AGENT_INTRO)
            .trim();
        if !intro.is_empty() {
            sections.push(intro.to_string());
        }
        let mut guidance = Vec::new();
        if !self.prompt.omit_builtin_guidance {
            section(
                &mut sections,
                "Execution",
                [standard_execution_section(self.behaviour.batch)],
            );
            let mut bullets = GUIDANCE_BASE.to_vec();
            if catalog.tools.iter().any(|tool| tool.manifest.name == "ask") {
                bullets.insert(1, GUIDANCE_INTERACTIVE);
            }
            guidance.push(bullets.join("\n"));
        }
        guidance.extend(self.prompt.instructions.iter().cloned());
        section(&mut sections, "Guidance", guidance);
        section(
            &mut sections,
            "Tool modules",
            catalog.modules().map(|module| module.render_markdown()),
        );
        section(
            &mut sections,
            "Context",
            self.prompt.context.iter().cloned(),
        );
        sections.join("\n\n")
    }
}

fn section(sections: &mut Vec<String>, title: &str, parts: impl IntoIterator<Item = String>) {
    let parts = parts
        .into_iter()
        .filter_map(|part| {
            let text = part.trim();
            (!text.is_empty()).then(|| text.to_string())
        })
        .collect::<Vec<_>>();
    if !parts.is_empty() {
        sections.push(format!("## {title}\n\n{}", parts.join("\n\n")));
    }
}
