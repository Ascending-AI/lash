use std::collections::HashMap;

use crate::PromptContext;
use crate::plugin::PromptContribution;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptBuiltin {
    MainAgentIntro,
    ExecutionInstructions,
    CoreGuidance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptSlot {
    Intro,
    Execution,
    Guidance,
    ProjectInstructions,
    RuntimeContext,
    Environment,
}

impl PartialOrd for PromptSlot {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PromptSlot {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (*self as u8).cmp(&(*other as u8))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PromptTemplateEntry {
    Text { content: String },
    Builtin { builtin: PromptBuiltin },
    Slot { slot: PromptSlot },
}

impl PromptTemplateEntry {
    pub fn text(content: impl Into<String>) -> Self {
        Self::Text {
            content: content.into(),
        }
    }

    pub fn builtin(builtin: PromptBuiltin) -> Self {
        Self::Builtin { builtin }
    }

    pub fn slot(slot: PromptSlot) -> Self {
        Self::Slot { slot }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PromptTemplateSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<PromptTemplateEntry>,
}

impl PromptTemplateSection {
    pub fn new(title: Option<String>, entries: Vec<PromptTemplateEntry>) -> Self {
        Self { title, entries }
    }

    pub fn untitled(entries: Vec<PromptTemplateEntry>) -> Self {
        Self {
            title: None,
            entries,
        }
    }

    pub fn titled(title: impl Into<String>, entries: Vec<PromptTemplateEntry>) -> Self {
        Self {
            title: Some(title.into()),
            entries,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PromptTemplate {
    pub sections: Vec<PromptTemplateSection>,
}

impl PromptTemplate {
    pub fn new(sections: Vec<PromptTemplateSection>) -> Self {
        Self { sections }
    }

    pub fn render(&self, prompt: &PromptContext) -> String {
        let contributions = grouped_contributions(prompt);
        self.sections
            .iter()
            .filter_map(|section| render_section(section, prompt, &contributions))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

impl Default for PromptTemplate {
    fn default() -> Self {
        default_prompt_template()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PromptLayer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<PromptTemplate>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub slots: HashMap<PromptSlot, PromptSlotLayer>,
}

impl PromptLayer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.template.is_none() && self.slots.is_empty()
    }

    pub fn with_template(template: PromptTemplate) -> Self {
        Self {
            template: Some(template),
            slots: HashMap::new(),
        }
    }

    pub fn prompt_template(mut self, template: PromptTemplate) -> Self {
        self.template = Some(template);
        self
    }

    pub fn clear_template(mut self) -> Self {
        self.template = None;
        self
    }

    pub fn add_contribution(&mut self, contribution: PromptContribution) {
        self.slots
            .entry(contribution.slot)
            .or_default()
            .contributions
            .push(contribution);
    }

    pub fn with_contribution(mut self, contribution: PromptContribution) -> Self {
        self.add_contribution(contribution);
        self
    }

    pub fn replace_slot(
        &mut self,
        slot: PromptSlot,
        contributions: impl IntoIterator<Item = PromptContribution>,
    ) {
        self.slots.insert(
            slot,
            PromptSlotLayer {
                reset: true,
                contributions: normalize_slot_contributions(slot, contributions),
            },
        );
    }

    pub fn with_replaced_slot(
        mut self,
        slot: PromptSlot,
        contributions: impl IntoIterator<Item = PromptContribution>,
    ) -> Self {
        self.replace_slot(slot, contributions);
        self
    }

    pub fn clear_slot(&mut self, slot: PromptSlot) {
        self.slots.insert(
            slot,
            PromptSlotLayer {
                reset: true,
                contributions: Vec::new(),
            },
        );
    }

    pub fn with_cleared_slot(mut self, slot: PromptSlot) -> Self {
        self.clear_slot(slot);
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PromptSlotLayer {
    #[serde(default)]
    pub reset: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contributions: Vec<PromptContribution>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPromptLayer {
    pub template: PromptTemplate,
    pub contributions: Vec<PromptContribution>,
}

pub fn resolve_prompt_layers<'a>(
    layers: impl IntoIterator<Item = &'a PromptLayer>,
) -> ResolvedPromptLayer {
    let mut template = default_prompt_template();
    let mut contributions = Vec::new();
    for layer in layers {
        if let Some(next_template) = &layer.template {
            template = next_template.clone();
        }
        for (slot, slot_layer) in &layer.slots {
            if slot_layer.reset {
                contributions
                    .retain(|contribution: &PromptContribution| contribution.slot != *slot);
            }
            contributions.extend(normalize_slot_contributions(
                *slot,
                slot_layer.contributions.iter().cloned(),
            ));
        }
    }
    ResolvedPromptLayer {
        template,
        contributions,
    }
}

fn normalize_slot_contributions(
    slot: PromptSlot,
    contributions: impl IntoIterator<Item = PromptContribution>,
) -> Vec<PromptContribution> {
    contributions
        .into_iter()
        .map(|mut contribution| {
            contribution.slot = slot;
            contribution
        })
        .collect()
}

pub fn default_prompt_template() -> PromptTemplate {
    PromptTemplate::new(vec![
        PromptTemplateSection::untitled(vec![
            PromptTemplateEntry::builtin(PromptBuiltin::MainAgentIntro),
            PromptTemplateEntry::slot(PromptSlot::Intro),
        ]),
        PromptTemplateSection::titled(
            "Execution",
            vec![
                PromptTemplateEntry::builtin(PromptBuiltin::ExecutionInstructions),
                PromptTemplateEntry::slot(PromptSlot::Execution),
            ],
        ),
        PromptTemplateSection::titled(
            "Guidance",
            vec![
                PromptTemplateEntry::builtin(PromptBuiltin::CoreGuidance),
                PromptTemplateEntry::slot(PromptSlot::ProjectInstructions),
                PromptTemplateEntry::slot(PromptSlot::Guidance),
            ],
        ),
        PromptTemplateSection::titled(
            "Environment",
            vec![
                PromptTemplateEntry::slot(PromptSlot::RuntimeContext),
                PromptTemplateEntry::slot(PromptSlot::Environment),
            ],
        ),
    ])
}

pub const MAIN_AGENT_INTRO: &str = "You are an AI coding assistant piloting the lash harness.";

/// Core guidance delivered in the `## Guidance` section. Rendered
/// through [`render_core_guidance`] rather than inlined as a `const`
/// so we can drop interactive-only advice when the session has no
/// `ask` tool (autonomous `--print` runs, benchmarks, etc.). Rules that
/// depend on being able to talk to a user only make sense when that
/// channel exists.
const CORE_GUIDANCE_BASE: &[&str] = &[
    "- Be concise. Avoid filler, hedging, and performative tone.",
    "- Do not restate a conclusion you already stated. Once a fix location is identified, act on it in the same turn.",
    "- Prefer the simplest correct solution over cleverness or unnecessary abstraction.",
];

const CORE_GUIDANCE_INTERACTIVE_ONLY: &str =
    "- Take initiative when the user's intent is clear. Ask only when progress is blocked.";

pub fn render_core_guidance(prompt: &PromptContext) -> String {
    let mut bullets: Vec<&str> = CORE_GUIDANCE_BASE.to_vec();
    if prompt.has_tool("ask") {
        // Insert after the "Be concise" lead-in so the interactive-
        // only rule sits alongside the other core directives instead
        // of at the end.
        bullets.insert(1, CORE_GUIDANCE_INTERACTIVE_ONLY);
    }
    bullets.join("\n")
}

fn grouped_contributions<'a>(
    prompt: &'a PromptContext,
) -> HashMap<PromptSlot, Vec<&'a PromptContribution>> {
    let mut grouped: HashMap<PromptSlot, Vec<&'a PromptContribution>> = HashMap::new();
    for contribution in prompt.contributions.iter() {
        grouped
            .entry(contribution.slot)
            .or_default()
            .push(contribution);
    }
    for entries in grouped.values_mut() {
        entries.sort_by_key(|contribution| contribution.priority);
    }
    grouped
}

fn render_section(
    section: &PromptTemplateSection,
    prompt: &PromptContext,
    contributions: &HashMap<PromptSlot, Vec<&PromptContribution>>,
) -> Option<String> {
    let mut parts = Vec::new();
    for entry in &section.entries {
        match entry {
            PromptTemplateEntry::Text { content } => push_text(&mut parts, content),
            PromptTemplateEntry::Builtin { builtin } => {
                push_text(&mut parts, &render_builtin(*builtin, prompt))
            }
            PromptTemplateEntry::Slot { slot } => {
                if let Some(entries) = contributions.get(slot) {
                    for contribution in entries {
                        if let Some(rendered) = render_contribution(contribution) {
                            parts.push(rendered);
                        }
                    }
                }
            }
        }
    }

    if parts.is_empty() {
        return None;
    }

    let mut rendered = Vec::new();
    if let Some(title) = section
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        rendered.push(format!("## {title}"));
    }
    rendered.extend(parts);
    Some(rendered.join("\n\n"))
}

fn push_text(parts: &mut Vec<String>, text: &str) {
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        parts.push(trimmed.to_string());
    }
}

fn render_builtin(builtin: PromptBuiltin, prompt: &PromptContext) -> String {
    match builtin {
        PromptBuiltin::MainAgentIntro => MAIN_AGENT_INTRO.to_string(),
        PromptBuiltin::ExecutionInstructions => prompt.execution_prompt.to_string(),
        PromptBuiltin::CoreGuidance => render_core_guidance(prompt),
    }
}

fn render_contribution(contribution: &PromptContribution) -> Option<String> {
    let content = contribution.content.trim();
    if content.is_empty() {
        return None;
    }
    match contribution
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
    {
        Some(title) => Some(format!("### {title}\n\n{content}")),
        None => Some(content.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt() -> PromptContext {
        PromptContext {
            execution_prompt: std::sync::Arc::from("protocol execution"),
            ..PromptContext::default()
        }
    }

    #[test]
    fn default_template_renders_builtin_sections() {
        let mut ctx = prompt();
        ctx.tool_names = std::sync::Arc::new(vec!["ask".to_string()]);
        let text = default_prompt_template().render(&ctx);
        assert!(text.contains(MAIN_AGENT_INTRO));
        assert!(text.contains("## Execution"));
        assert!(text.contains("protocol execution"));
        assert!(text.contains("## Guidance"));
        // Interactive context: the "ask when blocked" guidance is in play.
        assert!(text.contains("Ask only when progress is blocked"));
    }

    #[test]
    fn core_guidance_drops_ask_line_when_ask_tool_absent() {
        // Autonomous `--print` / benchmark sessions filter out the
        // `ask` tool, so the guidance line telling the model "Ask
        // only when progress is blocked" would contradict the
        // run-time constraint. `render_core_guidance` must drop it.
        let ctx = prompt();
        assert!(!ctx.has_tool("ask"));
        let rendered = render_core_guidance(&ctx);
        assert!(rendered.contains("Be concise"));
        assert!(rendered.contains("Prefer the simplest correct solution"));
        assert!(!rendered.contains("Ask only when progress is blocked"));
    }

    #[test]
    fn core_guidance_keeps_ask_line_when_ask_tool_present() {
        let mut ctx = prompt();
        ctx.tool_names = std::sync::Arc::new(vec!["ask".to_string()]);
        let rendered = render_core_guidance(&ctx);
        assert!(rendered.contains("Ask only when progress is blocked"));
    }

    #[test]
    fn template_renders_slot_contributions_in_order() {
        let mut prompt = prompt();
        prompt.contributions = vec![
            PromptContribution::guidance("Second Guide", "Second details.").with_priority(10),
            PromptContribution::guidance("First Guide", "First details.").with_priority(0),
        ]
        .into();
        let text = default_prompt_template().render(&prompt);
        assert!(text.contains("### First Guide"));
        assert!(text.contains("### Second Guide"));
        assert!(text.find("### First Guide").unwrap() < text.find("### Second Guide").unwrap());
    }

    #[test]
    fn template_can_omit_builtin_guidance_and_keep_plugin_guidance() {
        let template = PromptTemplate::new(vec![PromptTemplateSection::titled(
            "Guidance",
            vec![PromptTemplateEntry::slot(PromptSlot::Guidance)],
        )]);
        let mut prompt = prompt();
        prompt.contributions =
            vec![PromptContribution::guidance("Custom", "More guidance.")].into();
        let text = template.render(&prompt);
        assert!(text.contains("## Guidance"));
        assert!(text.contains("### Custom"));
        // Template with no `CoreGuidance` builtin omits the baked-in
        // guidance lines — only plugin contributions should land.
        assert!(!text.contains("Be concise. Avoid filler"));
    }

    #[test]
    fn template_can_place_project_instructions_separately() {
        let template = PromptTemplate::new(vec![
            PromptTemplateSection::titled(
                "Rules",
                vec![PromptTemplateEntry::slot(PromptSlot::ProjectInstructions)],
            ),
            PromptTemplateSection::titled(
                "Guidance",
                vec![PromptTemplateEntry::slot(PromptSlot::Guidance)],
            ),
        ]);
        let mut prompt = prompt();
        prompt.contributions = vec![
            PromptContribution::project_instructions("Repo rules"),
            PromptContribution::guidance("Shell", "Use exec_command."),
        ]
        .into();
        let text = template.render(&prompt);
        assert!(text.contains("## Rules"));
        assert!(text.contains("Repo rules"));
        assert!(text.contains("## Guidance"));
        assert!(text.contains("### Shell"));
    }

    #[test]
    fn empty_sections_are_skipped() {
        let template = PromptTemplate::new(vec![PromptTemplateSection::titled(
            "Environment",
            vec![PromptTemplateEntry::slot(PromptSlot::Environment)],
        )]);
        let text = template.render(&prompt());
        assert!(text.is_empty());
    }
}
