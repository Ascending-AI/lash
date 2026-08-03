use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use crate::llm::types::LlmToolSpec;
use crate::{
    PromptContribution, PromptFingerprint, ToolContract, ToolDefinition, ToolManifest,
    prompt_tool_names_fingerprint,
};

pub type ToolContractResolver =
    Arc<dyn Fn(&str) -> Option<Arc<ToolContract>> + Send + Sync + 'static>;

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
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct ToolCatalog {
    pub tools: Vec<ToolCatalogEntry>,
    #[serde(skip)]
    resolve_contract: Option<ToolContractResolver>,
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
            resolve_contract: self.resolve_contract.clone(),
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
            resolve_contract: None,
            model_tool_specs: OnceLock::new(),
            tool_names: OnceLock::new(),
            tool_names_fingerprint: OnceLock::new(),
        }
    }
}

impl ToolCatalog {
    pub fn from_tool_definitions(tools: Vec<ToolDefinition>) -> Self {
        let contracts = tools
            .iter()
            .map(|tool| (tool.name().to_string(), Arc::new(tool.contract())))
            .collect();
        Self::from_tools(
            tools.into_iter().map(|tool| tool.manifest()).collect(),
            contracts,
        )
    }

    pub fn from_tools(
        tools: Vec<ToolManifest>,
        contracts: BTreeMap<String, Arc<ToolContract>>,
    ) -> Self {
        let resolver_contracts = Arc::new(contracts);
        Self::from_tool_manifests(
            tools,
            Some(Arc::new(move |name| resolver_contracts.get(name).cloned())),
        )
    }

    fn from_tool_manifests(
        tools: Vec<ToolManifest>,
        resolve_contract: Option<ToolContractResolver>,
    ) -> Self {
        Self {
            tools: tools
                .into_iter()
                .map(|manifest| ToolCatalogEntry { manifest })
                .collect(),
            resolve_contract,
            model_tool_specs: OnceLock::new(),
            tool_names: OnceLock::new(),
            tool_names_fingerprint: OnceLock::new(),
        }
    }

    pub(crate) fn callable_tools_iter(&self) -> impl Iterator<Item = &ToolManifest> {
        self.tools.iter().map(|tool| &tool.manifest)
    }

    pub fn callable_tools(&self) -> Vec<ToolManifest> {
        self.callable_tools_iter().cloned().collect()
    }

    /// Membership test: a tool is in the catalog (callable) or it does not
    /// exist to the model.
    pub fn has_callable_tool(&self, tool_name: &str) -> bool {
        self.tools
            .iter()
            .any(|tool| tool.manifest.name == tool_name)
    }

    pub fn tool_names(&self) -> Arc<Vec<String>> {
        Arc::clone(self.tool_names.get_or_init(|| {
            Arc::new(
                self.tools
                    .iter()
                    .map(|tool| tool.manifest.name.clone())
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
                    .filter_map(|tool| {
                        self.resolve_contract(&tool.manifest.name)
                            .map(|contract| contract.model_tool(&tool.manifest))
                    })
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

    pub fn resolve_contract(&self, tool_name: &str) -> Option<Arc<ToolContract>> {
        self.resolve_contract
            .as_ref()
            .and_then(|resolve| resolve(tool_name))
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

pub fn build_tool_catalog(input: ToolCatalogBuildInput) -> ToolCatalog {
    let mut catalog = ToolCatalog::from_tool_manifests(input.tools, input.resolve_contract);
    for contribution in input.contributions {
        apply_contribution(&mut catalog, contribution);
    }
    catalog
}

fn apply_contribution(catalog: &mut ToolCatalog, contribution: ToolCatalogContribution) {
    if contribution.remove.is_empty() {
        return;
    }
    catalog
        .tools
        .retain(|tool| !contribution.remove.contains(&tool.manifest.name));
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
            .map(|tool| (tool.name().to_string(), Arc::new(tool.contract())))
            .collect::<BTreeMap<_, _>>();
        ToolCatalogBuildInput {
            tools: tools.into_iter().map(|tool| tool.manifest()).collect(),
            resolve_contract: Some(Arc::new(move |name| contracts.get(name).cloned())),
            contributions,
        }
    }

    #[test]
    fn catalog_membership_is_flat_and_callable() {
        let catalog = build_tool_catalog(build_input(
            vec![tool("read_file"), tool("grep"), tool("write_file")],
            Vec::new(),
        ));

        assert_eq!(catalog.callable_tools().len(), 3);
        assert!(catalog.has_callable_tool("read_file"));
        assert!(catalog.has_callable_tool("grep"));
        assert!(!catalog.has_callable_tool("absent"));
    }

    #[test]
    fn contributions_remove_members() {
        let catalog = build_tool_catalog(build_input(
            vec![tool("read_file"), tool("write_file")],
            vec![ToolCatalogContribution::remove_tools(["write_file"])],
        ));

        assert!(catalog.has_callable_tool("read_file"));
        assert!(!catalog.has_callable_tool("write_file"));
        assert_eq!(catalog.callable_tools().len(), 1);
    }

    #[test]
    fn prompt_gate_requires_member_tool() {
        let catalog = build_tool_catalog(build_input(vec![tool("read_file")], Vec::new()));

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
    fn model_specs_resolve_lazily() {
        let contract_resolutions = Arc::new(AtomicUsize::new(0));
        let callable = tool("read_file");
        let resolver_count = Arc::clone(&contract_resolutions);
        let catalog = build_tool_catalog(ToolCatalogBuildInput {
            tools: vec![callable.manifest()],
            resolve_contract: Some(Arc::new(move |name| {
                resolver_count.fetch_add(1, Ordering::SeqCst);
                (name == "read_file").then(|| Arc::new(callable.contract()))
            })),
            contributions: Vec::new(),
        });

        assert_eq!(contract_resolutions.load(Ordering::SeqCst), 0);
        assert_eq!(catalog.model_tool_specs().len(), 1);
        assert_eq!(contract_resolutions.load(Ordering::SeqCst), 1);
        assert_eq!(catalog.model_tool_specs().len(), 1);
        assert_eq!(contract_resolutions.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn tool_names_fingerprint_matches_prompt_hash() {
        let catalog = build_tool_catalog(build_input(
            vec![tool("read_file"), tool("grep")],
            Vec::new(),
        ));

        assert_eq!(
            catalog.tool_names_fingerprint(),
            prompt_tool_names_fingerprint(&catalog.tool_names())
        );
    }
}
