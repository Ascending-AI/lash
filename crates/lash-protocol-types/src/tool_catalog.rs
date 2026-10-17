use std::any::Any;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::llm::types::LlmToolSpec;
use crate::sync::MutexExt;
use crate::{ToolContract, ToolDefinition, ToolManifest};

pub type ToolContractResolver =
    Arc<dyn Fn(&ToolManifest) -> Option<Arc<ToolContract>> + Send + Sync + 'static>;

#[derive(Clone)]
pub struct ToolCatalogBuildInput {
    pub tools: Vec<ToolManifest>,
    pub resolve_contract: Option<ToolContractResolver>,
    pub contributions: Vec<ToolCatalogContribution>,
    /// The kinds of the process engines registered where the catalog is
    /// built: an isolated member enters only where its engine is among them.
    pub isolation_engines: std::collections::BTreeSet<String>,
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
    derived_documents: DerivedDocuments,
}

/// Documents a downstream crate derives from exactly this membership, one per
/// document type. Like the other memos, a clone carries the ones already
/// derived, so every copy of one catalog generation derives each only once.
#[derive(Default)]
struct DerivedDocuments(Mutex<Vec<DerivedDocument>>);

type DerivedDocument = Arc<dyn Any + Send + Sync>;

fn find_derived<T: Any + Send + Sync>(documents: &[DerivedDocument]) -> Option<Arc<T>> {
    documents
        .iter()
        .find_map(|document| Arc::clone(document).downcast::<T>().ok())
}

impl Clone for DerivedDocuments {
    fn clone(&self) -> Self {
        Self(Mutex::new(self.0.lock_recover().clone()))
    }
}

impl Clone for ToolCatalog {
    fn clone(&self) -> Self {
        let clone = Self {
            tools: self.tools.clone(),
            model_tool_specs: OnceLock::new(),
            tool_names: OnceLock::new(),
            derived_documents: self.derived_documents.clone(),
        };
        if let Some(value) = self.model_tool_specs.get() {
            let _ = clone.model_tool_specs.set(Arc::clone(value));
        }
        if let Some(value) = self.tool_names.get() {
            let _ = clone.tool_names.set(Arc::clone(value));
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
            derived_documents: DerivedDocuments::default(),
        }
    }
}

impl ToolCatalog {
    /// Module metadata from this catalog's recorded members, once per name.
    /// Filtering membership also filters modules; no live provider is read.
    pub fn modules(&self) -> impl Iterator<Item = &crate::ToolModule> {
        let mut names = std::collections::BTreeSet::new();
        self.tools.iter().filter_map(move |entry| {
            let module = entry.manifest.module.as_deref()?;
            names.insert(module.name.as_str()).then_some(module)
        })
    }

    /// Prompt-only projection. The original catalogue retains execution authority.
    pub fn inline_tools(&self) -> Self {
        self.filtered(|entry| entry.manifest.inline)
    }

    /// The members `keep` accepts, as a new catalog that derives its own
    /// documents rather than inheriting this one's.
    pub fn filtered(&self, mut keep: impl FnMut(&ToolCatalogEntry) -> bool) -> Self {
        Self::from_entries(
            self.tools
                .iter()
                .filter(|entry| keep(entry))
                .cloned()
                .collect(),
        )
    }

    /// The document of type `T` derived from this catalog's membership,
    /// computed by `derive` on first request and shared by every clone after.
    ///
    /// The document is a function of `tools` alone, so a caller that edits
    /// `tools` in place must start from a fresh catalog ([`Self::filtered`])
    /// rather than a clone.
    pub fn derived<T: Any + Send + Sync>(&self, derive: impl FnOnce(&Self) -> T) -> Arc<T> {
        if let Some(document) = find_derived(&self.derived_documents.0.lock_recover()) {
            return document;
        }
        // Derived outside the lock; a racing derivation of the same document
        // yields to whichever landed first.
        let document = Arc::new(derive(self));
        let mut documents = self.derived_documents.0.lock_recover();
        if let Some(existing) = find_derived(&documents) {
            return existing;
        }
        documents.push(Arc::clone(&document) as DerivedDocument);
        document
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
            isolation_engines: std::collections::BTreeSet::new(),
        })
    }

    fn from_entries(tools: Vec<ToolCatalogEntry>) -> Self {
        Self {
            tools,
            model_tool_specs: OnceLock::new(),
            tool_names: OnceLock::new(),
            derived_documents: DerivedDocuments::default(),
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

    pub fn model_tool_specs(&self) -> Arc<Vec<LlmToolSpec>> {
        Arc::clone(self.model_tool_specs.get_or_init(|| {
            Arc::new(
                self.tools
                    .iter()
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
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub enum ToolCatalogBuildError {
    UnusableSchema {
        tool_id: crate::ToolId,
        name: String,
        purpose: crate::SchemaPurpose,
        source: crate::SchemaAdmissionError,
    },
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
    /// A member is refused against the deployment it registers in.
    RegistrationRefused {
        refusal: crate::RegistrationRefused,
    },
}

impl std::fmt::Display for ToolCatalogBuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnusableSchema {
                tool_id,
                name,
                purpose,
                source,
            } => {
                write!(
                    formatter,
                    "tool `{name}` ({tool_id}) has unusable {purpose:?} schema: {source}"
                )
            }
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
            Self::RegistrationRefused { refusal } => write!(formatter, "{refusal}"),
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
        if let Some(engine) = manifest.isolation_engine()
            && !input.isolation_engines.contains(engine)
        {
            return Err(ToolCatalogBuildError::RegistrationRefused {
                refusal: crate::RegistrationRefused::UnregisteredIsolationEngine {
                    tool: manifest.name.clone(),
                    engine: engine.to_owned(),
                },
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
            // A control call's result is its control: a tool that declares
            // one has no output for a contract to state beside it.
            if !manifest.declaration().controls.is_empty() && !contract.states_no_output() {
                return Err(ToolCatalogBuildError::RegistrationRefused {
                    refusal: crate::RegistrationRefused::ControlDeclaresOutput {
                        tool: manifest.name.clone(),
                    },
                });
            }
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition::raw(
            format!("tool:{name}"),
            name,
            format!("Tool {name}"),
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            serde_json::json!({ "type": "string" }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(30))
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
            isolation_engines: std::collections::BTreeSet::new(),
        }
    }

    #[test]
    fn schema_admission_refuses_defects_before_catalog_or_value_validation() {
        for schema in [
            serde_json::json!(null),
            serde_json::json!(42),
            serde_json::json!("string"),
            serde_json::json!([]),
            serde_json::json!({ "type": "unknown" }),
            serde_json::json!({ "$ref": "https://example.invalid/schema" }),
            serde_json::json!({ "$ref": "#/$defs/Missing" }),
        ] {
            let encoded = serde_json::json!({ "canonical": schema });
            assert!(
                serde_json::from_value::<crate::SchemaContract>(encoded).is_err(),
                "schema defect entered as a contract: {schema}"
            );
        }
    }

    #[test]
    fn schema_admission_keeps_typed_cause_and_value_mismatch_separate() {
        let literal_reference = serde_json::json!({
            "type": "object",
            "properties": {"$ref": {"type": "string"}},
            "default": {"$ref": "https://example.invalid/default"},
            "enum": [{"$ref": "https://example.invalid/value"}]
        });
        let admitted = crate::JsonSchema::admit(literal_reference.clone())
            .expect("reference keys inside literal data are not schema references");
        assert_eq!(admitted.as_value(), &literal_reference);
        assert!(
            admitted
                .validate(&serde_json::json!({
                    "$ref": "https://example.invalid/value"
                }))
                .is_ok()
        );
        let error = ToolDefinition::raw(
            "bad",
            "bad",
            "bad",
            serde_json::json!({"type": "unknown"}),
            serde_json::json!({}),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ToolCatalogBuildError::UnusableSchema {
                purpose: crate::SchemaPurpose::ToolInput,
                source: crate::SchemaAdmissionError::Compilation { .. },
                ..
            }
        ));
        let schema = crate::JsonSchema::admit(
            serde_json::json!({"type": "object", "properties": {"count": {"type": "integer"}}}),
        )
        .unwrap();
        let mismatch = schema
            .validate(&serde_json::json!({"count": "bad"}))
            .unwrap_err();
        assert_eq!(mismatch.instance_path, "/count");
        assert!(!mismatch.message.is_empty());
        let clone = schema.clone();
        assert_eq!(
            clone.validate(&serde_json::json!({"count": "bad"})),
            Err(mismatch)
        );
        assert_eq!(
            serde_json::to_value(crate::SchemaContract::default()).unwrap(),
            serde_json::json!({"canonical": {}})
        );
        assert_eq!(
            crate::JsonSchema::admit(serde_json::json!(true)).unwrap(),
            crate::JsonSchema::any()
        );
        assert!(
            crate::JsonSchema::admit(serde_json::json!(false))
                .unwrap()
                .validate(&serde_json::json!(null))
                .is_err()
        );
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
    fn missing_contract_is_refused_only_for_effective_members() {
        let missing = tool("missing").manifest();
        let hidden = build_tool_catalog(ToolCatalogBuildInput {
            tools: vec![missing.clone()],
            resolve_contract: None,
            contributions: vec![ToolCatalogContribution::remove_tools(["missing"])],
            isolation_engines: std::collections::BTreeSet::new(),
        })
        .expect("suppressed manifests are not resident members");
        assert!(hidden.tools.is_empty());

        let error = build_tool_catalog(ToolCatalogBuildInput {
            tools: vec![missing.clone()],
            resolve_contract: None,
            contributions: Vec::new(),
            isolation_engines: std::collections::BTreeSet::new(),
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
            isolation_engines: std::collections::BTreeSet::new(),
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
                )
                .expect("valid declared tool schemas")
                .with_execution(std::time::Duration::from_secs(30)),
                ToolDefinition::raw(
                    repeated_name.id,
                    repeated_name.name,
                    "Second",
                    ToolDefinition::default_input_schema(),
                    serde_json::json!({ "type": "string" }),
                )
                .expect("valid declared tool schemas")
                .with_execution(std::time::Duration::from_secs(30)),
            ],
            Vec::new(),
        ))
        .expect_err("a duplicate effective name is ambiguous model authority");
        assert_eq!(
            error,
            ToolCatalogBuildError::DuplicateName { name: first.name }
        );
    }

    fn refused(
        result: Result<ToolDefinition, crate::RegistrationRefused>,
    ) -> crate::RegistrationRefused {
        result.expect_err("the tool was defined")
    }

    /// FIG-5410 law 4, FIG-5774: a tool missing a bound its host must set
    /// cannot be defined. An `execution` is what makes a draft a definition;
    /// the `park` of a tool that may defer is refused typed when it is
    /// absent, and when a tool that never defers carries one. A stored
    /// manifest without its bounds does not decode. Lash supplies neither.
    #[test]
    fn a_tool_missing_a_host_set_bound_cannot_be_defined() {
        assert_eq!(
            refused(tool("approve").with_declaration(crate::ToolDeclaration::deferring(), None)),
            crate::RegistrationRefused::MissingBound {
                tool: "approve".into(),
                bound: crate::ToolBound::Park,
            }
        );
        assert_eq!(
            refused(tool("read").with_declaration(
                crate::ToolDeclaration::default(),
                Some(crate::ParkBound::UntilScopeEnd)
            )),
            crate::RegistrationRefused::ParkWithoutDeferral {
                tool: "read".into(),
            }
        );
        let stored = |edit: fn(&mut serde_json::Value)| {
            let mut manifest = serde_json::to_value(
                tool("approve")
                    .with_declaration(
                        crate::ToolDeclaration::deferring(),
                        Some(crate::ParkBound::UntilScopeEnd),
                    )
                    .expect("a deferring tool with a park bound")
                    .manifest(),
            )
            .expect("a manifest encodes");
            edit(&mut manifest);
            serde_json::from_value::<ToolManifest>(manifest)
                .expect_err("a manifest without its bounds decoded")
                .to_string()
        };
        assert_eq!(
            stored(|manifest| drop(manifest["execution"].take())),
            "tool `approve` declares no execution bound; its host must set one"
        );
        assert_eq!(
            stored(|manifest| drop(manifest["park"].take())),
            "tool `approve` declares no park bound; its host must set one"
        );

        let hour = std::time::Duration::from_secs(60 * 60);
        let approve = tool("approve")
            .with_declaration(
                crate::ToolDeclaration::deferring(),
                Some(crate::ParkBound::Within(hour)),
            )
            .expect("a deferring tool with a park bound");
        let decoded: ToolManifest = serde_json::from_value(
            serde_json::to_value(approve.manifest()).expect("a manifest encodes"),
        )
        .expect("a complete manifest decodes");
        let execution = std::time::Duration::from_secs(30);
        assert_eq!(
            decoded.bounds(),
            crate::ToolBounds {
                execution,
                park: Some(crate::ParkBound::Within(hour)),
            }
        );
        assert_eq!(
            tool("read").manifest.bounds(),
            crate::ToolBounds {
                execution,
                park: None,
            }
        );
    }

    /// FIG-5774: a tool with an invalid declaration cannot be defined, and a
    /// stored manifest holding one does not decode.
    #[test]
    fn a_tool_with_an_invalid_declaration_cannot_be_defined() {
        let unordered = crate::ToolDeclaration {
            intents: vec![
                crate::ToolIntentKind::CancelProcess,
                crate::ToolIntentKind::StartProcess,
            ],
            ..crate::ToolDeclaration::default()
        };
        assert_eq!(
            refused(tool("spawn").with_declaration(unordered.clone(), None)),
            crate::RegistrationRefused::Declaration {
                tool: "spawn".into(),
                cause: crate::DeclarationRefusal::IntentOrder,
            }
        );
        let deferring = tool("spawn")
            .with_declaration(
                crate::ToolDeclaration::deferring(),
                Some(crate::ParkBound::UntilScopeEnd),
            )
            .expect("a deferring tool with a park bound");
        assert_eq!(
            refused(deferring.isolated_in("sandbox")),
            crate::RegistrationRefused::Declaration {
                tool: "spawn".into(),
                cause: crate::DeclarationRefusal::IsolatedInlineCapability,
            }
        );
        let mut stored =
            serde_json::to_value(tool("spawn").manifest()).expect("a manifest encodes");
        stored["declaration"] = serde_json::to_value(unordered).expect("a declaration encodes");
        assert_eq!(
            serde_json::from_value::<ToolManifest>(stored)
                .expect_err("a manifest with an invalid declaration decoded")
                .to_string(),
            "tool `spawn`'s declaration is refused: declared intents are not in vocabulary order"
        );
    }

    /// FIG-5774: an isolated tool names the process engine its calls run in,
    /// and enters a catalog only where that engine is registered.
    #[test]
    fn an_isolated_tool_registers_only_where_its_engine_is_registered() {
        let isolated = crate::ToolDeclaration {
            isolated: true,
            ..crate::ToolDeclaration::default()
        };
        assert_eq!(
            refused(tool("run").with_declaration(isolated, None)),
            crate::RegistrationRefused::IsolatedWithoutEngine { tool: "run".into() }
        );
        let run = tool("run")
            .isolated_in("sandbox")
            .expect("an isolated tool naming its engine");
        assert_eq!(run.manifest.isolation_engine(), Some("sandbox"));
        let register = |engines: &[&str]| {
            build_tool_catalog(ToolCatalogBuildInput {
                isolation_engines: engines.iter().map(|kind| (*kind).to_owned()).collect(),
                ..build_input(vec![run.clone(), tool("read")], Vec::new())
            })
        };
        for engines in [&[][..], &["other"][..]] {
            assert_eq!(
                register(engines).expect_err("the isolated tool was registered"),
                ToolCatalogBuildError::RegistrationRefused {
                    refusal: crate::RegistrationRefused::UnregisteredIsolationEngine {
                        tool: "run".into(),
                        engine: "sandbox".into(),
                    },
                }
            );
        }
        let catalog = register(&["other", "sandbox"]).expect("its engine is registered");
        assert!(catalog.has_callable_tool("run"));
    }
}
