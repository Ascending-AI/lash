use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use crate::llm::types::LlmToolSpec;
use crate::{
    PromptContribution, PromptFingerprint, ToolActivation, ToolContract, ToolDefinition,
    ToolManifest, prompt_tool_names_fingerprint,
};

pub type ToolContractResolver =
    Arc<dyn Fn(&ToolManifest) -> Option<Arc<ToolContract>> + Send + Sync + 'static>;

#[derive(Clone)]
pub struct ToolCatalogBuildInput {
    pub tools: Vec<ToolManifest>,
    pub resolve_contract: Option<ToolContractResolver>,
    pub contributions: Vec<ToolCatalogContribution>,
}

/// A trusted plugin's contribution to catalog assembly. Membership is the
/// execution gate, so the only override a contribution can express is *removal*
/// of a member (authority hiding, plan-mode gating). Adding members happens by
/// a [`crate::ToolProvider`] including them in its manifest list.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ToolCatalogContribution {
    /// Names of tools to remove from the catalog (non-membership).
    pub remove: Vec<String>,
}

impl ToolCatalogContribution {
    pub fn is_empty(&self) -> bool {
        self.remove.is_empty()
    }

    pub fn remove_tools(tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            remove: tools.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ToolCatalogEntry {
    pub manifest: ToolManifest,
    pub contract: Arc<ToolContract>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ToolCatalog {
    pub tools: Vec<ToolCatalogEntry>,
    #[serde(skip)]
    model_tool_specs: OnceLock<Arc<Vec<LlmToolSpec>>>,
    #[serde(skip)]
    tool_names: OnceLock<Arc<Vec<String>>>,
    #[serde(skip)]
    tool_names_fingerprint: OnceLock<PromptFingerprint>,
}

impl Clone for ToolCatalog {
    fn clone(&self) -> Self {
        let clone = Self {
            tools: self.tools.clone(),
            model_tool_specs: OnceLock::new(),
            tool_names: OnceLock::new(),
            tool_names_fingerprint: OnceLock::new(),
        };
        if let Some(value) = self.model_tool_specs.get() {
            let _ = clone.model_tool_specs.set(Arc::clone(value));
        }
        if let Some(value) = self.tool_names.get() {
            let _ = clone.tool_names.set(Arc::clone(value));
        }
        if let Some(value) = self.tool_names_fingerprint.get() {
            let _ = clone.tool_names_fingerprint.set(*value);
        }
        clone
    }
}

impl std::fmt::Debug for ToolCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCatalog")
            .field("tools", &self.tools)
            .finish_non_exhaustive()
    }
}

impl Default for ToolCatalog {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            model_tool_specs: OnceLock::new(),
            tool_names: OnceLock::new(),
            tool_names_fingerprint: OnceLock::new(),
        }
    }
}

impl ToolCatalog {
    /// Prompt-only projection. The original catalogue retains execution authority.
    pub fn inline_tools(&self) -> Self {
        let tools = self
            .tools
            .iter()
            .filter(|entry| entry.manifest.inline)
            .cloned()
            .collect();
        Self::from_entries(tools)
    }

    pub fn from_tool_definitions(tools: Vec<ToolDefinition>) -> Self {
        Self::from_entries(
            tools
                .into_iter()
                .map(|tool| ToolCatalogEntry {
                    manifest: tool.manifest,
                    contract: Arc::new(tool.contract),
                })
                .collect(),
        )
    }

    pub fn from_tools(
        tools: Vec<ToolManifest>,
        contracts: BTreeMap<crate::ToolId, Arc<ToolContract>>,
    ) -> Result<Self, ToolCatalogBuildError> {
        let resolver_contracts = Arc::new(contracts);
        build_tool_catalog(ToolCatalogBuildInput {
            tools,
            resolve_contract: Some(Arc::new(move |manifest| {
                resolver_contracts.get(&manifest.id).cloned()
            })),
            contributions: Vec::new(),
        })
    }

    fn from_entries(tools: Vec<ToolCatalogEntry>) -> Self {
        Self {
            tools,
            model_tool_specs: OnceLock::new(),
            tool_names: OnceLock::new(),
            tool_names_fingerprint: OnceLock::new(),
        }
    }

    pub(crate) fn callable_tools_iter(&self) -> impl Iterator<Item = &ToolManifest> {
        self.tools
            .iter()
            .map(|tool| &tool.manifest)
            .filter(|manifest| manifest.activation != ToolActivation::Internal)
    }

    pub fn callable_tools(&self) -> Vec<ToolManifest> {
        self.callable_tools_iter().cloned().collect()
    }

    /// Membership test: a tool is in the catalog (callable) or it does not
    /// exist to the model.
    pub fn has_callable_tool(&self, tool_name: &str) -> bool {
        self.callable_tools_iter()
            .any(|manifest| manifest.name == tool_name)
    }

    pub fn tool_names(&self) -> Arc<Vec<String>> {
        Arc::clone(self.tool_names.get_or_init(|| {
            Arc::new(
                self.callable_tools_iter()
                    .map(|manifest| manifest.name.clone())
                    .collect(),
            )
        }))
    }

    pub fn tool_names_fingerprint(&self) -> PromptFingerprint {
        *self
            .tool_names_fingerprint
            .get_or_init(|| prompt_tool_names_fingerprint(&self.tool_names()))
    }

    pub fn model_tool_specs(&self) -> Arc<Vec<LlmToolSpec>> {
        Arc::clone(self.model_tool_specs.get_or_init(|| {
            Arc::new(
                self.tools
                    .iter()
                    .filter(|tool| tool.manifest.activation != ToolActivation::Internal)
                    .map(|tool| tool.contract.model_tool(&tool.manifest))
                    .map(|model_tool| LlmToolSpec {
                        name: model_tool.name,
                        description: model_tool.description,
                        input_schema: model_tool.input_schema,
                        output_schema: model_tool.output_schema,
                    })
                    .collect(),
            )
        }))
    }

    pub(crate) fn filter_prompt_contributions(
        &self,
        contributions: Vec<PromptContribution>,
    ) -> Vec<PromptContribution> {
        contributions
            .into_iter()
            .filter(|contribution| self.includes_prompt_contribution(contribution))
            .collect()
    }

    fn includes_prompt_contribution(&self, contribution: &PromptContribution) -> bool {
        if contribution.gate.is_empty() {
            return true;
        }
        contribution
            .gate
            .tools
            .iter()
            .any(|tool_name| self.has_callable_tool(tool_name))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolCatalogBuildError {
    MissingContract {
        tool_id: crate::ToolId,
        name: String,
    },
    DuplicateId {
        tool_id: crate::ToolId,
    },
    DuplicateName {
        name: String,
    },
}

impl std::fmt::Display for ToolCatalogBuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingContract { tool_id, name } => {
                write!(
                    formatter,
                    "resident tool `{name}` ({tool_id}) has no contract"
                )
            }
            Self::DuplicateId { tool_id } => {
                write!(formatter, "resident catalog repeats tool id `{tool_id}`")
            }
            Self::DuplicateName { name } => {
                write!(formatter, "resident catalog repeats tool name `{name}`")
            }
        }
    }
}

impl std::error::Error for ToolCatalogBuildError {}

pub fn build_tool_catalog(
    input: ToolCatalogBuildInput,
) -> Result<ToolCatalog, ToolCatalogBuildError> {
    let mut tools = input.tools;
    for contribution in input.contributions {
        apply_contribution(&mut tools, contribution);
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut names = std::collections::BTreeSet::new();
    for manifest in &tools {
        if !ids.insert(&manifest.id) {
            return Err(ToolCatalogBuildError::DuplicateId {
                tool_id: manifest.id.clone(),
            });
        }
        if !names.insert(manifest.name.as_str()) {
            return Err(ToolCatalogBuildError::DuplicateName {
                name: manifest.name.clone(),
            });
        }
    }
    let entries = tools
        .into_iter()
        .map(|manifest| {
            let contract = input
                .resolve_contract
                .as_ref()
                .and_then(|resolve| resolve(&manifest))
                .ok_or_else(|| ToolCatalogBuildError::MissingContract {
                    tool_id: manifest.id.clone(),
                    name: manifest.name.clone(),
                })?;
            Ok(ToolCatalogEntry { manifest, contract })
        })
        .collect::<Result<_, _>>()?;
    Ok(ToolCatalog::from_entries(entries))
}

fn apply_contribution(tools: &mut Vec<ToolManifest>, contribution: ToolCatalogContribution) {
    if contribution.remove.is_empty() {
        return;
    }
    tools.retain(|tool| !contribution.remove.contains(&tool.name));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolActivation;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn tool(name: &str) -> ToolDefinition {
        let mut definition = ToolDefinition::raw(
            format!("tool:{name}"),
            name,
            format!("Tool {name}"),
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            serde_json::json!({ "type": "string" }),
        );
        definition.manifest.activation = ToolActivation::Always;
        definition
    }

    fn build_input(
        tools: Vec<ToolDefinition>,
        contributions: Vec<ToolCatalogContribution>,
    ) -> ToolCatalogBuildInput {
        let contracts = tools
            .iter()
            .map(|tool| (tool.manifest.id.clone(), Arc::new(tool.contract())))
            .collect::<BTreeMap<_, _>>();
        ToolCatalogBuildInput {
            tools: tools.into_iter().map(|tool| tool.manifest()).collect(),
            resolve_contract: Some(Arc::new(move |manifest| {
                contracts.get(&manifest.id).cloned()
            })),
            contributions,
        }
    }

    #[test]
    fn catalog_membership_is_flat_and_callable() {
        let catalog = build_tool_catalog(build_input(
            vec![tool("read_file"), tool("grep"), tool("write_file")],
            Vec::new(),
        ))
        .expect("complete resident definitions");

        assert_eq!(catalog.callable_tools().len(), 3);
        assert!(catalog.has_callable_tool("read_file"));
        assert!(catalog.has_callable_tool("grep"));
        assert!(!catalog.has_callable_tool("absent"));
    }

    #[test]
    fn internal_members_are_resolvable_but_never_model_callable() {
        let mut internal = tool("internal_runner");
        internal.manifest.activation = ToolActivation::Internal;
        let internal_contract = Arc::new(internal.contract());
        let catalog =
            build_tool_catalog(build_input(vec![tool("read_file"), internal], Vec::new()))
                .expect("complete resident definitions");

        assert_eq!(catalog.tools.len(), 2);
        assert_eq!(catalog.tools[1].manifest.name, "internal_runner");
        assert_eq!(catalog.tools[1].contract, internal_contract);
        assert!(!catalog.has_callable_tool("internal_runner"));
        assert_eq!(catalog.tool_names().as_ref(), &["read_file".to_string()]);
        assert_eq!(catalog.model_tool_specs().len(), 1);
    }

    #[test]
    fn contributions_remove_members() {
        let catalog = build_tool_catalog(build_input(
            vec![tool("read_file"), tool("write_file")],
            vec![ToolCatalogContribution::remove_tools(["write_file"])],
        ))
        .expect("complete effective definition");

        assert!(catalog.has_callable_tool("read_file"));
        assert!(!catalog.has_callable_tool("write_file"));
        assert_eq!(catalog.callable_tools().len(), 1);
    }

    #[test]
    fn prompt_gate_requires_member_tool() {
        let catalog = build_tool_catalog(build_input(vec![tool("read_file")], Vec::new()))
            .expect("complete resident definition");

        let kept = catalog.filter_prompt_contributions(vec![
            PromptContribution::guidance("Plain", "always"),
            PromptContribution::guidance("WithTool", "withtool").requires_tool("read_file"),
            PromptContribution::guidance("MissingTool", "missing").requires_tool("missing_tool"),
        ]);

        assert_eq!(kept.len(), 2);
        assert!(
            kept.iter()
                .any(|contribution| contribution.title.as_deref() == Some("Plain"))
        );
        assert!(
            kept.iter()
                .any(|contribution| contribution.title.as_deref() == Some("WithTool"))
        );
    }

    #[test]
    fn catalog_pins_contract_once_before_any_projection() {
        let contract_resolutions = Arc::new(AtomicUsize::new(0));
        let callable = tool("read_file");
        let resolver_count = Arc::clone(&contract_resolutions);
        let catalog = build_tool_catalog(ToolCatalogBuildInput {
            tools: vec![callable.manifest()],
            resolve_contract: Some(Arc::new(move |manifest| {
                resolver_count.fetch_add(1, Ordering::SeqCst);
                (manifest.id == callable.manifest.id).then(|| Arc::new(callable.contract()))
            })),
            contributions: Vec::new(),
        })
        .expect("resident definition is complete");

        assert_eq!(contract_resolutions.load(Ordering::SeqCst), 1);
        assert_eq!(catalog.model_tool_specs().len(), 1);
        assert_eq!(contract_resolutions.load(Ordering::SeqCst), 1);
        assert_eq!(catalog.model_tool_specs().len(), 1);
        assert_eq!(contract_resolutions.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn missing_contract_is_refused_only_for_effective_members() {
        let missing = tool("missing").manifest();
        let hidden = build_tool_catalog(ToolCatalogBuildInput {
            tools: vec![missing.clone()],
            resolve_contract: None,
            contributions: vec![ToolCatalogContribution::remove_tools(["missing"])],
        })
        .expect("suppressed manifests are not resident members");
        assert!(hidden.tools.is_empty());

        let error = build_tool_catalog(ToolCatalogBuildInput {
            tools: vec![missing.clone()],
            resolve_contract: None,
            contributions: Vec::new(),
        })
        .expect_err("an effective resident member requires a contract");
        assert_eq!(
            error,
            ToolCatalogBuildError::MissingContract {
                tool_id: missing.id,
                name: "missing".into(),
            }
        );
    }

    #[test]
    fn duplicate_effective_identity_is_refused_before_contract_resolution() {
        let first = tool("first").manifest();
        let mut repeated_id = tool("second").manifest();
        repeated_id.id = first.id.clone();
        let resolutions = Arc::new(AtomicUsize::new(0));
        let resolver_count = Arc::clone(&resolutions);
        let error = build_tool_catalog(ToolCatalogBuildInput {
            tools: vec![first.clone(), repeated_id],
            resolve_contract: Some(Arc::new(move |_| {
                resolver_count.fetch_add(1, Ordering::SeqCst);
                None
            })),
            contributions: Vec::new(),
        })
        .expect_err("a duplicate effective ToolId is ambiguous authority");
        assert_eq!(
            error,
            ToolCatalogBuildError::DuplicateId {
                tool_id: first.id.clone(),
            }
        );
        assert_eq!(resolutions.load(Ordering::SeqCst), 0);

        let mut repeated_name = tool("second").manifest();
        repeated_name.name = first.name.clone();
        let error = build_tool_catalog(build_input(
            vec![
                ToolDefinition::raw(
                    first.id,
                    first.name.clone(),
                    "First",
                    ToolDefinition::default_input_schema(),
                    serde_json::json!({ "type": "string" }),
                ),
                ToolDefinition::raw(
                    repeated_name.id,
                    repeated_name.name,
                    "Second",
                    ToolDefinition::default_input_schema(),
                    serde_json::json!({ "type": "string" }),
                ),
            ],
            Vec::new(),
        ))
        .expect_err("a duplicate effective name is ambiguous model authority");
        assert_eq!(
            error,
            ToolCatalogBuildError::DuplicateName { name: first.name }
        );
    }

    #[test]
    fn tool_names_fingerprint_matches_prompt_hash() {
        let catalog = build_tool_catalog(build_input(
            vec![tool("read_file"), tool("grep")],
            Vec::new(),
        ))
        .expect("complete resident definitions");

        assert_eq!(
            catalog.tool_names_fingerprint(),
            prompt_tool_names_fingerprint(&catalog.tool_names())
        );
    }
}
