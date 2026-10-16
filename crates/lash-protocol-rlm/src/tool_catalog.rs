use lash_core::plugin::{PluginError, ToolCatalogContext};
use lash_core::{SessionError, ToolCatalog, facade_support::ToolCatalogContribution};
use lash_tool_presenter_api::{
    PresentationRefusal, ToolDisclosure, ToolPresentationEntry, ToolPresentationRequest,
};
use lash_vm_runtime::{ToolBindingResolutionExt, required_tool_binding, required_tool_executable};

use crate::dialect::SessionDialect;

/// RLM catalog assembly. The catalog is a flat callable set: every member is
/// rendered as a full prompt doc under its call-path. RLM contributes
/// no removals; it validates that each member carries an explicit
/// `lash.tool` binding the session's dialect can call by module path.
pub(crate) fn rlm_tool_catalog(
    ctx: ToolCatalogContext,
    dialect: &SessionDialect,
) -> Result<ToolCatalogContribution, PluginError> {
    let _build_tool_catalog = lash_core::facade_support::build_tool_catalog;
    validate_rlm_language_bindings(&ctx.tools, dialect.language())?;
    // A host's own finish tool takes the place of `control.finish`.
    Ok(lash_core::suppress_default_finish(
        &ctx,
        &lash_core::ToolId::from(crate::control_tools::FINISH_TOOL_ID),
    ))
}

/// Every member's doc, in catalogue order: the listing of a session with no
/// discovery, where being a member *is* being presented.
#[cfg(test)]
pub(crate) fn rlm_prompt_tool_docs(
    tool_catalog: &ToolCatalog,
    dialect: &crate::dialect::SessionDialect,
    features: crate::protocol::RlmPromptFeatures,
) -> String {
    ToolListing::of(tool_catalog, dialect, features, None)
        .expect("the test catalogue presents")
        .docs
}

/// What a session's prompt lists of its catalogue.
pub(crate) struct ToolListing {
    /// The listed docs, as the session's tool presenter wrote them.
    pub(crate) docs: String,
    /// What a discovery session lists and leaves to discovery; `None`
    /// without discovery, where every member is listed.
    pub(crate) disclosure: Option<ToolCatalogueDisclosure>,
}

impl ToolListing {
    /// The catalogue as the session's tool presenter lists it. Without
    /// `discovery`, every member. With it, the discovery tool and the
    /// control tools, then what fits the recorded budget. The presenter
    /// words the listing; which tools are offered, and under which call
    /// paths, is this mode's, and a listing that does not account for
    /// exactly the offered tools is refused.
    #[expect(
        clippy::expect_used,
        reason = "catalog registration validates every tool binding against the session's dialect, so tool_call_path only errs on an unregistered manifest"
    )]
    pub(crate) fn of(
        tool_catalog: &ToolCatalog,
        dialect: &crate::dialect::SessionDialect,
        features: crate::protocol::RlmPromptFeatures,
        discovery: Option<&crate::plugin::RlmDiscovery>,
    ) -> Result<Self, SessionError> {
        let offered = tool_catalog
            .tools
            .iter()
            .filter(|tool| features.decomposition || tool.manifest.name != "continue_as")
            .map(|tool| {
                let call_path = dialect.tool_call_path(&tool.manifest).expect(
                    "RLM tool catalog registration validates the session dialect's binding",
                );
                (tool, call_path)
            })
            .collect::<Vec<_>>();
        let entries = offered
            .iter()
            .map(|(tool, call_path)| ToolPresentationEntry {
                manifest: &tool.manifest,
                contract: &tool.contract,
                call_path,
                pinned: discovery.is_some_and(|discovery| *call_path == discovery.operation)
                    || !tool.manifest.declaration().controls.is_empty(),
            })
            .collect::<Vec<_>>();
        let request = ToolPresentationRequest {
            tools: &entries,
            config: dialect.tool_presentation(),
            disclosure: match discovery {
                Some(discovery) => ToolDisclosure::Budget {
                    catalogue_bytes: discovery.catalogue_bytes,
                },
                None => ToolDisclosure::Complete,
            },
        };
        let presenter = dialect.tool_presenter().presenter();
        let section = presenter
            .present(request)
            .map_err(|refusal| SessionError::Protocol(refusal.to_string()))?;
        if !section.accounts_for(&request) {
            return Err(SessionError::Protocol(
                PresentationRefusal::Listing {
                    presenter: presenter.id().to_owned(),
                }
                .to_string(),
            ));
        }
        let names = |ids: &[lash_core::ToolId]| {
            entries
                .iter()
                .filter(|entry| ids.contains(&entry.manifest.id))
                .map(|entry| entry.manifest.name.clone())
                .collect()
        };
        Ok(Self {
            disclosure: discovery.map(|discovery| ToolCatalogueDisclosure {
                discovery_operation: discovery.operation.clone(),
                listed: names(&section.listed),
                omitted: names(&section.omitted),
            }),
            docs: section.text,
        })
    }
}

/// Which tools a discovery session's prompt lists and which it leaves for
/// its discovery operation to find (FIG-1435). The prompt labels its listing
/// complete or partial from these facts; a host that renders its own
/// catalogue reads them through [`tool_catalogue_disclosure`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCatalogueDisclosure {
    /// The call path of the host's discovery tool.
    pub discovery_operation: String,
    /// The listed tools by name, in catalogue order.
    pub listed: Vec<String>,
    /// The tools left to discovery by name, in catalogue order.
    pub omitted: Vec<String>,
}

impl ToolCatalogueDisclosure {
    /// Whether every tool is listed.
    pub fn is_complete(&self) -> bool {
        self.omitted.is_empty()
    }

    /// How many tools the catalogue offers the prompt.
    pub fn total(&self) -> usize {
        self.listed.len() + self.omitted.len()
    }
}

fn presentation_refused(refusal: impl std::fmt::Display) -> crate::dialect::DialectRefusal {
    crate::dialect::DialectRefusal {
        kind: crate::dialect::DialectRefusalKind::PresentationRefused,
        message: refusal.to_string(),
    }
}

/// `tool_presentation` admitted by `dialect`'s tool presenter: a host's raw
/// config, or a session's recorded one, which admits as itself.
fn admitted(
    dialect: &crate::dialect::CellDialect,
    tool_presentation: &serde_json::Value,
) -> Result<serde_json::Value, crate::dialect::DialectRefusal> {
    dialect
        .tool_presenter()
        .presenter()
        .admit_config(tool_presentation)
        .map_err(presentation_refused)
}

/// What a session in `dialect` with tool presentation `tool_presentation`
/// and `features` lists of `tool_catalog` under `discovery`: the facts the
/// protocol's declarations render, for a host that renders its catalogue
/// itself. `tool_catalog` is a session's validated catalogue: every member
/// has a binding `dialect` can call.
///
/// # Errors
///
/// The dialect's tool presenter refuses the config or the listing.
pub fn tool_catalogue_disclosure(
    dialect: &crate::dialect::CellDialect,
    tool_presentation: &serde_json::Value,
    features: crate::protocol::RlmPromptFeatures,
    tool_catalog: &ToolCatalog,
    discovery: &crate::plugin::RlmDiscovery,
) -> Result<ToolCatalogueDisclosure, crate::dialect::DialectRefusal> {
    let session = SessionDialect::prompt_only(dialect.clone())
        .with_tool_presentation(admitted(dialect, tool_presentation)?);
    Ok(
        ToolListing::of(tool_catalog, &session, features, Some(discovery))
            .map_err(presentation_refused)?
            .disclosure
            .unwrap_or_else(|| ToolCatalogueDisclosure {
                discovery_operation: discovery.operation.clone(),
                listed: Vec::new(),
                omitted: Vec::new(),
            }),
    )
}

/// A discovered tool's signature from its binding and admitted schemas in the
/// session's `dialect` and `tool_presentation`, without Markdown or
/// documentation. This is the signature line shown when the tool is loaded
/// into the prompt.
///
/// # Errors
///
/// The binding names a path `dialect` cannot call, or the dialect's tool
/// presenter refuses.
pub fn discovered_tool_signature(
    dialect: &crate::dialect::CellDialect,
    tool_presentation: &serde_json::Value,
    binding: &lash_core::ToolBinding,
    input: &lash_core::JsonSchema,
    output: &lash_core::JsonSchema,
) -> Result<String, crate::dialect::DialectRefusal> {
    let call_path = binding_call_path(dialect, binding)?;
    dialect
        .tool_presenter()
        .presenter()
        .tool_signature(
            &call_path,
            input,
            output,
            &admitted(dialect, tool_presentation)?,
        )
        .map_err(presentation_refused)
}

/// The call path `dialect` spells `binding` as.
fn binding_call_path(
    dialect: &crate::dialect::CellDialect,
    binding: &lash_core::ToolBinding,
) -> Result<String, crate::dialect::DialectRefusal> {
    let name = format!(
        "{}.{}",
        binding.module_path.join("."),
        binding.operation.as_deref().unwrap_or_default(),
    );
    let binding =
        binding
            .executable_for(&name)
            .map_err(|error| crate::dialect::DialectRefusal {
                kind: crate::dialect::DialectRefusalKind::UnaddressableToolPath,
                message: error.to_string(),
            })?;
    dialect.prompts().tool_call_path(&binding)
}

/// The call path `dialect` spells `tool`'s binding as.
fn tool_call_path(
    dialect: &crate::dialect::CellDialect,
    tool: &lash_core::ToolDefinition,
) -> Result<String, crate::dialect::DialectRefusal> {
    let binding =
        required_tool_binding(&tool.manifest).map_err(|error| crate::dialect::DialectRefusal {
            kind: crate::dialect::DialectRefusalKind::UnaddressableToolPath,
            message: error.to_string(),
        })?;
    binding_call_path(dialect, &binding)
}

/// A loaded tool's one-line signature, without Markdown or field notes.
/// This is the same signature [`discovered_tool_doc`] puts in its first line.
/// For a binding and schemas without a loaded definition, use
/// [`discovered_tool_signature`].
///
/// ```rust
/// use lash::rlm::{CellDialect, loaded_tool_signature};
/// use lash::tools::{ToolBinding, ToolDefinition, ToolDefinitionBindingExt};
/// use serde_json::json;
///
/// let tool = ToolDefinition::raw(
///     "lookup", "lookup", "Find a record.",
///     json!({
///         "type": "object",
///         "properties": { "query": { "type": "string" } },
///         "required": ["query"],
///         "additionalProperties": false
///     }),
///     json!({ "type": "string" }),
/// )?
/// .with_execution(std::time::Duration::from_secs(30))
/// .with_tool_binding(ToolBinding::from_path("tools.lookup"));
/// let signature = loaded_tool_signature(&CellDialect::typescript(), &json!({}), &tool)?;
/// assert_eq!(signature, "tools.lookup({ query: string }): Promise<string>");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
///
/// The tool has no binding, or one `dialect` cannot call, or the dialect's
/// tool presenter refuses.
pub fn loaded_tool_signature(
    dialect: &crate::dialect::CellDialect,
    tool_presentation: &serde_json::Value,
    tool: &lash_core::ToolDefinition,
) -> Result<String, crate::dialect::DialectRefusal> {
    let binding =
        required_tool_binding(&tool.manifest).map_err(|error| crate::dialect::DialectRefusal {
            kind: crate::dialect::DialectRefusalKind::UnaddressableToolPath,
            message: error.to_string(),
        })?;
    discovered_tool_signature(
        dialect,
        tool_presentation,
        &binding,
        &tool.contract.input_schema.canonical,
        &tool.contract.output_schema.canonical,
    )
}

/// A tool a host's discovery operation found, as a session in `dialect`
/// with `tool_presentation` shows it: the doc a listed tool has, its field
/// notes included, so a discovered signature that collapses a field to an
/// open record still says what the field holds (FIG-1435).
///
/// # Errors
///
/// The tool has no binding, or one `dialect` cannot call, or the dialect's
/// tool presenter refuses.
pub fn discovered_tool_doc(
    dialect: &crate::dialect::CellDialect,
    tool_presentation: &serde_json::Value,
    tool: &lash_core::ToolDefinition,
) -> Result<String, crate::dialect::DialectRefusal> {
    let call_path = tool_call_path(dialect, tool)?;
    dialect
        .tool_presenter()
        .presenter()
        .tool_doc(
            &ToolPresentationEntry {
                manifest: &tool.manifest,
                contract: &tool.contract,
                call_path: &call_path,
                pinned: false,
            },
            &admitted(dialect, tool_presentation)?,
        )
        .map_err(presentation_refused)
}

/// The text a host's discovery search matches a tool against, as `dialect`'s
/// tool presenter with `tool_presentation` writes it: its name, its
/// description, and every input and output field's path with its
/// description, one per line, so a query that names what a field holds
/// finds the tool (FIG-1435).
///
/// # Errors
///
/// The dialect's tool presenter refuses.
pub fn discovery_text(
    dialect: &crate::dialect::CellDialect,
    tool_presentation: &serde_json::Value,
    tool: &lash_core::ToolDefinition,
) -> Result<String, crate::dialect::DialectRefusal> {
    dialect
        .tool_presenter()
        .presenter()
        .discovery_text(
            &tool.manifest,
            &tool.contract,
            &admitted(dialect, tool_presentation)?,
        )
        .map_err(presentation_refused)
}

fn validate_rlm_language_bindings(
    tools: &[lash_core::ToolManifest],
    language: &dyn crate::dialect::DialectPrompts,
) -> Result<(), PluginError> {
    for tool in tools {
        let binding = required_tool_executable(tool)
            .map_err(|err| PluginError::Registration(err.to_string()))?;
        // Being a catalog member is being advertised under the call path the
        // dialect spells; a binding the dialect cannot address could only be
        // advertised as a callable nothing, so it is refused here instead.
        language.tool_call_path(&binding).map_err(|refusal| {
            PluginError::Registration(format!(
                "tool `{}` has a `lash.tool` binding the {} dialect cannot call: {refusal}",
                tool.name,
                language.prompt_vocabulary().language_name
            ))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::typescript_test_dialect;
    use lash_core::{
        ToolContract, ToolDefinition, facade_support::build_tool_catalog,
        test_support::ToolCatalogBuildInput,
    };
    use lash_sansio::SessionId;
    use lash_vm_runtime::{ToolBinding, ToolDefinitionBindingExt};
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn native_rlm_and_validation_share_one_pinned_definition_under_registry_drift() {
        let definition = ToolDefinition::raw(
            "tool:test/pinned",
            "pinned_tool",
            "Pinned schema authority",
            json!({
                "type": "object",
                "properties": { "pinned": { "type": "string" } },
                "required": ["pinned"],
                "additionalProperties": false
            }),
            json!({ "type": "string" }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
        .with_tool_binding(ToolBinding::new(["authority"], "pinned"));
        let drifted = ToolDefinition::raw(
            "tool:test/pinned",
            "pinned_tool",
            "Drifted provider schema",
            json!({
                "type": "object",
                "properties": { "drifted": { "type": "integer" } },
                "required": ["drifted"],
                "additionalProperties": false
            }),
            json!({ "type": "integer" }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
        .with_tool_binding(ToolBinding::new(["authority"], "pinned"));
        let resolutions = Arc::new(AtomicUsize::new(0));
        let resolution_count = Arc::clone(&resolutions);
        let catalog = build_tool_catalog(ToolCatalogBuildInput {
            tools: vec![definition.manifest()],
            resolve_contract: Some(Arc::new(move |_| {
                let generation = resolution_count.fetch_add(1, Ordering::SeqCst);
                Some(Arc::new(if generation == 0 {
                    definition.contract()
                } else {
                    drifted.contract()
                }))
            })),
            contributions: Vec::new(),
            isolation_engines: std::collections::BTreeSet::new(),
        })
        .expect("first resident definition is pinned");

        let native = catalog.model_tool_specs();
        assert!(
            native[0]
                .input_schema
                .canonical()
                .pointer("/properties/pinned")
                .is_some()
        );
        assert!(
            native[0]
                .input_schema
                .canonical()
                .pointer("/properties/drifted")
                .is_none()
        );
        let docs = rlm_prompt_tool_docs(
            &catalog,
            &typescript_test_dialect(),
            crate::protocol::RlmPromptFeatures::default(),
        );
        assert!(docs.contains("pinned"), "{docs}");
        // The effect a cell performs carries the pinned contract's types.
        let mut boundary = lash_vm_runtime::HostBoundary::new();
        boundary
            .offer_bound_tool(
                &catalog.tools[0].manifest,
                &catalog.tools[0].contract.input_schema.canonical,
                &catalog.tools[0].contract.output_schema.canonical,
            )
            .expect("pinned contract binds as an effect");
        let signatures = boundary.signatures();
        let signature = format!("{:?}", signatures.values().next().expect("one effect"));
        assert!(
            signature.contains("pinned") && !signature.contains("drifted"),
            "{signature}"
        );
        assert!(
            lash_sansio::validate_tool_input(
                &catalog.tools[0].contract,
                &json!({ "pinned": "yes" })
            )
            .is_ok()
        );
        assert!(
            lash_sansio::validate_tool_input(&catalog.tools[0].contract, &json!({ "drifted": 1 }))
                .is_err()
        );
        assert_eq!(
            resolutions.load(Ordering::SeqCst),
            1,
            "every projection and validation reuses the captured contract"
        );
    }

    #[test]
    fn rlm_catalog_rejects_members_without_tool_binding() {
        let missing = ToolDefinition::raw(
            "tool:test/update_plan",
            "update_plan",
            "Update plan",
            ToolContract::default_input_schema(),
            json!({ "type": "string" }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120));

        let err = rlm_tool_catalog(
            ToolCatalogContext {
                owner: lash_core::RuntimeOwner::Session(SessionId::from("session")),
                tools: vec![missing.manifest()],
                config: lash_core::OwnerConfigView::default(),
                extensions: Default::default(),
            },
            &typescript_test_dialect(),
        )
        .expect_err("missing binding should fail RLM registration");

        assert!(
            err.to_string()
                .contains("missing an explicit `lash.tool` binding"),
            "{err}"
        );
    }

    /// The retired `lash_vm.tool` key is not a reader alias: a manifest that
    /// carries only it has no binding at all, and registration must say so
    /// rather than silently accepting the dead key (FIG-3273).
    #[test]
    fn rlm_catalog_rejects_members_with_only_retired_binding_key() {
        let mut retired_only = ToolDefinition::raw(
            "tool:test/update_plan",
            "update_plan",
            "Update plan",
            ToolContract::default_input_schema(),
            json!({ "type": "string" }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
        .with_tool_binding(ToolBinding::new(["plan"], "update"));
        let binding = retired_only
            .manifest
            .bindings
            .remove(lash_core::facade_support::TOOL_BINDING_KEY)
            .expect("with_tool_binding wrote the canonical key");
        retired_only
            .manifest
            .bindings
            .insert("lash_vm.tool".to_string(), binding);

        let err = rlm_tool_catalog(
            ToolCatalogContext {
                owner: lash_core::RuntimeOwner::Session(SessionId::from("session")),
                tools: vec![retired_only.manifest()],
                config: lash_core::OwnerConfigView::default(),
                extensions: Default::default(),
            },
            &typescript_test_dialect(),
        )
        .expect_err("a manifest carrying only `lash_vm.tool` must fail RLM registration");

        assert!(
            err.to_string()
                .contains("missing an explicit `lash.tool` binding"),
            "{err}"
        );
    }

    #[test]
    fn member_rlm_tool_docs_render_and_link_module_call() {
        let update_plan = ToolDefinition::raw(
            "tool:test/update_plan",
            "update_plan",
            "Update the visible plan",
            json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step": { "type": "string" },
                                "status": { "type": "string" }
                            },
                            "required": ["step", "status"]
                        }
                    }
                },
                "required": ["plan"],
                "additionalProperties": false
            }),
            json!({ "type": "string" }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
        .with_tool_binding(ToolBinding::new(["plan"], "update"));

        let contracts: std::collections::BTreeMap<_, _> = [update_plan.clone()]
            .iter()
            .map(|tool| (tool.manifest.id.clone(), Arc::new(tool.contract())))
            .collect();
        let manifests = vec![update_plan.manifest()];
        let contribution = rlm_tool_catalog(
            ToolCatalogContext {
                owner: lash_core::RuntimeOwner::Session(SessionId::from("session")),
                tools: manifests.clone(),
                config: lash_core::OwnerConfigView::default(),
                extensions: Default::default(),
            },
            &typescript_test_dialect(),
        )
        .expect("RLM catalog validates explicit binding");
        let catalog = build_tool_catalog(ToolCatalogBuildInput {
            tools: manifests,
            resolve_contract: Some(Arc::new(move |manifest| {
                contracts.get(&manifest.id).cloned()
            })),
            contributions: vec![contribution],
            isolation_engines: std::collections::BTreeSet::new(),
        })
        .expect("complete resident definitions");

        let docs = rlm_prompt_tool_docs(
            &catalog,
            &typescript_test_dialect(),
            crate::protocol::RlmPromptFeatures::default(),
        );
        assert!(docs.len() <= 768, "plan.update docs exceeded budget");
        assert!(docs.contains("plan.update("), "{docs}");
        assert!(
            docs.contains("plan: Array<{ step: string; status: string }>"),
            "{docs}"
        );
        assert!(!docs.contains("update_plan("), "{docs}");

        // The advertised path is the effect a cell performs.
        let mut boundary = lash_vm_runtime::HostBoundary::new();
        boundary
            .offer_bound_tool(
                &catalog.tools[0].manifest,
                &catalog.tools[0].contract.input_schema.canonical,
                &catalog.tools[0].contract.output_schema.canonical,
            )
            .expect("explicit binding binds as an effect");
        assert!(
            boundary
                .signatures()
                .keys()
                .any(|effect| effect.as_str() == "plan.update")
        );
    }

    /// FIG-4544. An MCP server hands its schemas through as written, and
    /// almost none of them say `additionalProperties: false`. The tool docs
    /// must still show every field's name and type, nested ones included,
    /// whether or not the field has a description.
    #[test]
    fn an_mcp_style_open_schema_renders_every_field_name_and_type() {
        let search = ToolDefinition::raw(
            "tool:mcp/issues_search",
            "mcp__tracker__issues_search",
            "[MCP tracker] Search issues.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 1 },
                    "filter": {
                        "type": "object",
                        "properties": {
                            "state": { "enum": ["open", "closed"] },
                            "labels": { "type": "array", "items": { "type": "string" } }
                        }
                    },
                    "sort": {
                        "type": "array",
                        "description": "Sort keys, most significant first.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "field": { "type": "string" },
                                "descending": { "type": "boolean", "default": false }
                            },
                            "required": ["field"]
                        }
                    },
                    "page": { "type": ["integer", "null"], "minimum": 1 }
                },
                "required": ["query"]
            }),
            json!({
                "type": "object",
                "properties": {
                    "issues": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "integer", "description": "Stable issue id." },
                                "title": { "type": "string" }
                            },
                            "required": ["id", "title"]
                        }
                    }
                },
                "required": ["issues"]
            }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
        .with_tool_binding(ToolBinding::new(["tracker"], "issues_search"));
        let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![search]);

        let docs = rlm_prompt_tool_docs(
            &catalog,
            &typescript_test_dialect(),
            crate::protocol::RlmPromptFeatures::default(),
        );
        assert_eq!(
            docs,
            concat!(
                "`tracker.issues_search({ query: string; ",
                "filter?: { labels?: Array<string>; state?: \"open\" | \"closed\" }; ",
                "page?: number | null; ",
                "sort?: Array<{ field: string; descending?: boolean }> }): ",
                "Promise<{ issues: Array<{ id: number; title: string }> }>`\n",
                "[MCP tracker] Search issues.\n",
                "Parameters:\n",
                "- `query: string` (min length 1)\n",
                "- `page?: number | null` (>= 1)\n",
                "- `sort?: Array<Record<string, unknown>>` — Sort keys, most significant first.\n",
                "- `sort[].descending?: boolean` (default false)\n",
                "Return fields:\n",
                "- `issues[].id: number` — Stable issue id."
            )
        );
    }

    fn tool_with_prose(description: &str, schema_description: &str) -> ToolDefinition {
        ToolDefinition::raw(
            "tool:test/spawn_agent",
            "spawn_agent",
            description,
            json!({
                "type": "object",
                "properties": {
                    "output": { "type": "object", "description": schema_description }
                },
                "additionalProperties": false
            }),
            json!({ "type": "string" }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
        .with_tool_binding(ToolBinding::new(["agents"], "spawn"))
    }

    #[test]
    fn typescript_tool_prose_is_rendered_verbatim() {
        let description = "Run a TypeScript child session in a <typescript> cell.";
        let schema_description = "A TypeScript process definition value.";
        let tool = tool_with_prose(description, schema_description);
        let catalog = build_tool_catalog(ToolCatalogBuildInput {
            tools: vec![tool.manifest()],
            resolve_contract: Some(Arc::new({
                let contract = Arc::new(tool.contract());
                move |_| Some(Arc::clone(&contract))
            })),
            contributions: vec![ToolCatalogContribution::default()],
            isolation_engines: std::collections::BTreeSet::new(),
        })
        .expect("complete resident definition");
        let docs = rlm_prompt_tool_docs(
            &catalog,
            &typescript_test_dialect(),
            crate::protocol::RlmPromptFeatures::default(),
        );
        assert!(docs.contains(description), "{docs}");
        assert!(docs.contains(schema_description), "{docs}");
    }
}

pub(crate) fn validate_discovery(
    tools: &[lash_core::ToolManifest],
    discovery: Option<&crate::plugin::RlmDiscovery>,
    dialect: &crate::dialect::SessionDialect,
) -> Result<(), PluginError> {
    if let Some(discovery) = discovery
        && !tools.iter().any(|tool| {
            tool.inline
                && dialect.tool_call_path(tool).ok().as_deref()
                    == Some(discovery.operation.as_str())
        })
    {
        return Err(PluginError::InvalidToolDiscovery {
            operation: discovery.operation.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod discovery_tests {
    use super::*;
    use lash_vm_runtime::{ToolBinding, ToolDefinitionBindingExt};
    #[test]
    fn discovery_filters_each_dialect_and_channel_and_requires_an_inline_operation() {
        let tool = |name: &str, inline| {
            let mut tool = lash_core::ToolDefinition::raw(
                name,
                name,
                format!("Description for {name}"),
                serde_json::json!({"type":"object"}),
                serde_json::json!({"type":"string"}),
            )
            .expect("valid declared tool schemas")
            .with_execution(std::time::Duration::from_secs(120))
            .with_tool_binding(ToolBinding::new(["tools"], name));
            tool.manifest.inline = inline;
            tool
        };
        let catalog = ToolCatalog::from_tool_definitions(vec![
            tool("search", true),
            tool("hidden", false),
            tool("visible", true),
        ]);
        let manifests = catalog
            .tools
            .iter()
            .map(|entry| entry.manifest.clone())
            .collect::<Vec<_>>();
        {
            let dialect = crate::dialect::typescript_test_dialect();
            for native in [false, true] {
                for discovery in [
                    None,
                    Some(crate::plugin::RlmDiscovery {
                        operation: "tools.search".into(),
                        catalogue_bytes: usize::MAX,
                    }),
                ] {
                    validate_discovery(&manifests, discovery.as_ref(), &dialect).unwrap();
                    let visible = catalog.clone();
                    let execution = if native {
                        crate::native::prompt::execution_section(
                            &dialect,
                            Default::default(),
                            &visible,
                            discovery.as_ref(),
                        )
                        .expect("the catalogue presents")
                        .joined()
                    } else {
                        dialect
                            .render_execution_section(
                                Default::default(),
                                &visible,
                                crate::plugin::RlmChannel::Cell,
                                discovery.as_ref(),
                            )
                            .unwrap()
                    };
                    let text = execution;
                    let docs =
                        ToolListing::of(&visible, &dialect, Default::default(), discovery.as_ref())
                            .expect("the catalogue presents")
                            .docs;
                    assert!(docs.contains("Description for search"));
                    assert!(docs.contains("Description for visible"));
                    assert_eq!(docs.contains("Description for hidden"), discovery.is_none());
                    assert_eq!(
                        text.contains("find the other 1 with `await tools.search"),
                        discovery.is_some()
                    );
                    assert!(!text.contains("Use discovery if available"));
                }
            }
            for operation in ["tools.hidden", "tools.absent"] {
                assert!(matches!(
                    validate_discovery(
                        &manifests,
                        Some(&crate::plugin::RlmDiscovery {
                            operation: operation.into(),
                            catalogue_bytes: usize::MAX,
                        }),
                        &dialect
                    ),
                    Err(PluginError::InvalidToolDiscovery { .. })
                ));
            }
        }
    }
}

#[cfg(test)]
mod listing_tests;
#[cfg(test)]
mod presenter_tests;
