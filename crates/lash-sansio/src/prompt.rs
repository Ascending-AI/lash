use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use crate::sync::MutexExt;

use crate::{PromptContribution, PromptTemplate};

/// Process-local cache identity for prompt inputs.
///
/// This intentionally uses Rust's default hasher because it only keys in-memory
/// prompt caches inside one process. Do not persist or compare it across
/// processes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PromptFingerprint(u64);

impl PromptFingerprint {
    fn from_hashable(value: impl Hash) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        value.hash(&mut hasher);
        Self(hasher.finish())
    }

    fn write(self, state: &mut impl Hasher) {
        self.0.hash(state);
    }
}

#[derive(Clone, Debug)]
pub struct PromptContributionSet {
    contributions: Arc<Vec<PromptContribution>>,
    fingerprint: PromptFingerprint,
}

impl PromptContributionSet {
    pub fn new(contributions: Vec<PromptContribution>) -> Self {
        let contributions = Arc::new(merge_prompt_contributions(contributions));
        let fingerprint = fingerprint_contributions(&contributions);
        Self {
            contributions,
            fingerprint,
        }
    }

    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    pub fn as_arc(&self) -> Arc<Vec<PromptContribution>> {
        Arc::clone(&self.contributions)
    }

    pub fn as_slice(&self) -> &[PromptContribution] {
        &self.contributions
    }

    pub fn fingerprint(&self) -> PromptFingerprint {
        self.fingerprint
    }
}

impl Default for PromptContributionSet {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Clone, Debug)]
pub struct PromptBuildInput {
    pub template: PromptTemplate,
    pub template_fingerprint: PromptFingerprint,
    pub execution_prompt: Arc<str>,
    pub execution_prompt_fingerprint: PromptFingerprint,
    pub tool_names: Arc<Vec<String>>,
    pub tool_names_fingerprint: PromptFingerprint,
    pub contributions: PromptContributionSet,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct PromptContext {
    #[serde(default)]
    pub execution_prompt: Arc<str>,
    pub tool_names: Arc<Vec<String>>,
    pub contributions: Arc<Vec<PromptContribution>>,
}

impl PromptContext {
    pub fn has_tool(&self, tool_name: &str) -> bool {
        self.tool_names.iter().any(|name| name == tool_name)
    }
}

#[derive(Clone, Debug)]
pub struct PreparedPrompt {
    pub context: PromptContext,
    pub system_prompt: Arc<str>,
}

/// Single-slot memo for the rendered system prompt, keyed by a hash of
/// the inputs. Most consecutive turns in a session pass identical
/// inputs (template, turn-driver-preamble-derived `execution_prompt` /
/// `tool_names`, plus context contributions), so a one-slot cache hits
/// repeatedly and avoids the section-by-section `Vec<String>::join`
/// work in `PromptTemplate::render`.
#[derive(Default)]
pub struct PromptCache {
    inner: Mutex<Option<(u64, Arc<str>)>>,
}

impl PromptCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&self) {
        *self.inner.lock_recover() = None;
    }
}

impl std::fmt::Debug for PromptCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptCache").finish_non_exhaustive()
    }
}

pub fn build_prompt(input: PromptBuildInput) -> PreparedPrompt {
    build_prompt_cached(input, None)
}

pub fn build_prompt_cached(input: PromptBuildInput, cache: Option<&PromptCache>) -> PreparedPrompt {
    let context = PromptContext {
        execution_prompt: Arc::clone(&input.execution_prompt),
        tool_names: Arc::clone(&input.tool_names),
        contributions: input.contributions.as_arc(),
    };
    let key = cache.map(|_| hash_prompt_inputs(&input));
    if let (Some(cache), Some(key)) = (cache, key)
        && let Some(cached) = cache
            .inner
            .lock_recover()
            .as_ref()
            .filter(|(k, _)| *k == key)
            .map(|(_, v)| Arc::clone(v))
    {
        return PreparedPrompt {
            context,
            system_prompt: cached,
        };
    }
    let system_prompt: Arc<str> = Arc::from(input.template.render(&context));
    if let (Some(cache), Some(key)) = (cache, key) {
        let mut guard = cache.inner.lock_recover();
        *guard = Some((key, Arc::clone(&system_prompt)));
    }
    PreparedPrompt {
        context,
        system_prompt,
    }
}

pub fn prompt_template_fingerprint(template: &PromptTemplate) -> PromptFingerprint {
    PromptFingerprint::from_hashable(template)
}

pub fn prompt_text_fingerprint(text: &str) -> PromptFingerprint {
    PromptFingerprint::from_hashable(text)
}

pub fn prompt_tool_names_fingerprint(tool_names: &[String]) -> PromptFingerprint {
    PromptFingerprint::from_hashable(tool_names)
}

fn hash_prompt_inputs(input: &PromptBuildInput) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    input.template_fingerprint.write(&mut hasher);
    input.execution_prompt_fingerprint.write(&mut hasher);
    input.tool_names_fingerprint.write(&mut hasher);
    input.contributions.fingerprint().write(&mut hasher);
    hasher.finish()
}

fn fingerprint_contributions(contributions: &[PromptContribution]) -> PromptFingerprint {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for contribution in contributions {
        contribution.slot.hash(&mut hasher);
        contribution.priority.hash(&mut hasher);
        contribution.title.hash(&mut hasher);
        contribution.content.hash(&mut hasher);
    }
    PromptFingerprint(hasher.finish())
}

fn merge_prompt_contributions(contributions: Vec<PromptContribution>) -> Vec<PromptContribution> {
    let mut merged = contributions
        .into_iter()
        .filter_map(normalize_contribution)
        .collect::<Vec<_>>();

    merged.sort_by(|left, right| {
        slot_order(left.slot)
            .cmp(&slot_order(right.slot))
            .then(left.priority.cmp(&right.priority))
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.content.cmp(&right.content))
    });

    // Duplicates are adjacent after the sort, so `dedup_by` on &str
    // refs drops them without cloning anything.
    merged.dedup_by(|a, b| {
        slot_order(a.slot) == slot_order(b.slot)
            && a.priority == b.priority
            && a.title.as_deref() == b.title.as_deref()
            && a.content == b.content
    });
    merged
}

fn normalize_contribution(mut contribution: PromptContribution) -> Option<PromptContribution> {
    contribution.content = Arc::from(contribution.content.trim());
    if contribution.content.is_empty() {
        return None;
    }
    contribution.title = contribution
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(Arc::from);
    Some(contribution)
}

fn slot_order(slot: crate::PromptSlot) -> usize {
    match slot {
        crate::PromptSlot::Intro => 0,
        crate::PromptSlot::Execution => 1,
        crate::PromptSlot::Guidance => 2,
        crate::PromptSlot::ProjectInstructions => 3,
        crate::PromptSlot::RuntimeContext => 4,
        crate::PromptSlot::Environment => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        PromptBuiltin, PromptContribution, PromptLayer, PromptSlot, PromptTemplate,
        PromptTemplateEntry, PromptTemplateSection, default_prompt_template, resolve_prompt_layers,
    };

    fn input(
        template: PromptTemplate,
        execution_prompt: &str,
        tool_names: Vec<String>,
        contributions: Vec<PromptContribution>,
    ) -> PromptBuildInput {
        let execution_prompt: Arc<str> = Arc::from(execution_prompt);
        let tool_names = Arc::new(tool_names);
        PromptBuildInput {
            template_fingerprint: prompt_template_fingerprint(&template),
            template,
            execution_prompt_fingerprint: prompt_text_fingerprint(&execution_prompt),
            execution_prompt,
            tool_names_fingerprint: prompt_tool_names_fingerprint(&tool_names),
            tool_names,
            contributions: PromptContributionSet::new(contributions),
        }
    }

    #[test]
    fn build_prompt_renders_template_from_merged_context() {
        let prepared = build_prompt(input(
            default_prompt_template(),
            "Use tools.",
            vec!["read_file".to_string()],
            vec![
                PromptContribution::guidance("Repo", "Follow repo rules."),
                PromptContribution::guidance("Repo", "Follow repo rules."),
                PromptContribution::project_instructions("Be careful."),
            ],
        ));

        assert!(prepared.system_prompt.contains("Use tools."));
        assert!(prepared.system_prompt.contains("Follow repo rules."));
        assert!(prepared.system_prompt.contains("Be careful."));
        assert_eq!(prepared.context.contributions.len(), 2);
    }

    #[test]
    fn build_prompt_cached_reuses_arc_on_identical_inputs() {
        let cache = PromptCache::new();
        let inputs = || {
            input(
                default_prompt_template(),
                "Use tools.",
                vec!["read_file".to_string()],
                vec![PromptContribution::guidance("Repo", "Follow repo rules.")],
            )
        };
        let first = build_prompt_cached(inputs(), Some(&cache));
        let second = build_prompt_cached(inputs(), Some(&cache));
        assert!(Arc::ptr_eq(&first.system_prompt, &second.system_prompt));
    }

    #[test]
    fn build_prompt_cached_renders_again_when_inputs_change() {
        let cache = PromptCache::new();
        let first = build_prompt_cached(
            input(
                default_prompt_template(),
                "Use tools.",
                vec!["read_file".to_string()],
                vec![],
            ),
            Some(&cache),
        );
        let second = build_prompt_cached(
            input(
                default_prompt_template(),
                "Use other tools.",
                vec!["read_file".to_string()],
                vec![],
            ),
            Some(&cache),
        );
        assert!(!Arc::ptr_eq(&first.system_prompt, &second.system_prompt));
        assert_ne!(first.system_prompt, second.system_prompt);
    }

    fn template_with_text(text: &str) -> PromptTemplate {
        PromptTemplate::new(vec![PromptTemplateSection::untitled(vec![
            PromptTemplateEntry::text(text),
            PromptTemplateEntry::builtin(PromptBuiltin::ExecutionInstructions),
        ])])
    }

    fn content(contributions: &[PromptContribution]) -> Vec<&str> {
        contributions
            .iter()
            .map(|contribution| contribution.content.as_ref())
            .collect()
    }

    #[test]
    fn prompt_layers_use_later_template() {
        let core = PromptLayer::with_template(template_with_text("core"));
        let session = PromptLayer::with_template(template_with_text("session"));
        let resolved = resolve_prompt_layers([&core, &session]);

        let rendered = resolved.template.render(&PromptContext {
            execution_prompt: Arc::from("execute"),
            ..PromptContext::default()
        });
        assert!(rendered.contains("session"));
        assert!(!rendered.contains("core"));
    }

    #[test]
    fn prompt_layers_append_inherited_slot_content() {
        let core =
            PromptLayer::new().with_contribution(PromptContribution::guidance("Core", "core"));
        let session = PromptLayer::new()
            .with_contribution(PromptContribution::guidance("Session", "session"));

        let resolved = resolve_prompt_layers([&core, &session]);
        assert_eq!(content(&resolved.contributions), vec!["core", "session"]);
    }

    #[test]
    fn prompt_layers_clear_one_slot_without_touching_others() {
        let core = PromptLayer::new()
            .with_contribution(PromptContribution::guidance("Guide", "guide"))
            .with_contribution(PromptContribution::project_instructions("project"));
        let session = PromptLayer::new().with_cleared_slot(PromptSlot::Guidance);

        let resolved = resolve_prompt_layers([&core, &session]);
        assert_eq!(content(&resolved.contributions), vec!["project"]);
    }

    #[test]
    fn prompt_layers_replace_slot_and_normalize_contribution_slot() {
        let core =
            PromptLayer::new().with_contribution(PromptContribution::guidance("Guide", "old"));
        let session = PromptLayer::new().with_replaced_slot(
            PromptSlot::Guidance,
            [PromptContribution::project_instructions("new")],
        );

        let resolved = resolve_prompt_layers([&core, &session]);
        assert_eq!(content(&resolved.contributions), vec!["new"]);
        assert_eq!(resolved.contributions[0].slot, PromptSlot::Guidance);
    }

    #[test]
    fn prompt_layers_allow_later_append_after_replace() {
        let core =
            PromptLayer::new().with_contribution(PromptContribution::guidance("Guide", "old"));
        let session = PromptLayer::new().with_replaced_slot(
            PromptSlot::Guidance,
            [PromptContribution::guidance("New", "new")],
        );
        let turn =
            PromptLayer::new().with_contribution(PromptContribution::guidance("Turn", "turn"));

        let resolved = resolve_prompt_layers([&core, &session, &turn]);
        assert_eq!(content(&resolved.contributions), vec!["new", "turn"]);
    }
}
