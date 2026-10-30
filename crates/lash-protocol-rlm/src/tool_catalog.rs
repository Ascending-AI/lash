use lash_core::plugin::ToolCatalogContext;
use lash_core::{SessionError, ToolCatalog, facade_support::ToolCatalogContribution};
use lash_execution_contracts::plugin_error::PluginError;
use lash_tool_presenter_api::{
    PresentationRefusal, ToolDisclosure, ToolPresentationEntry, ToolPresentationRequest,
};

use crate::dialect::SessionDialect;

/// RLM catalog assembly. The catalog is a flat callable set: every member is
/// rendered as a full prompt doc under its operation's path, which every
/// dialect can write (through the qualified escape where a segment is one
/// of its keywords). RLM contributes no removals.
pub(crate) fn rlm_tool_catalog(
    ctx: ToolCatalogContext,
    _dialect: &SessionDialect,
) -> Result<ToolCatalogContribution, PluginError> {
    // A host's own finish tool takes the place of `control.finish`.
    Ok(lash_core::suppress_default_finish(
        &ctx,
        &lash_vocabulary::tool_contract::ToolId::from(crate::control_tools::FINISH_TOOL_ID),
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
#[derive(Clone)]
pub(crate) struct ToolListing {
    /// The listed docs, as the session's tool presenter wrote them.
    pub(crate) docs: String,
    /// What a discovery session lists and leaves to discovery; `None`
    /// without discovery, where every member is listed.
    pub(crate) disclosure: Option<ToolCatalogueDisclosure>,
}

#[derive(Default)]
struct Listings(std::sync::Mutex<std::collections::VecDeque<(ListingKey, ToolListing)>>);

struct ListingKey {
    catalog: Vec<lash_core::ToolCatalogEntry>,
    dialect: crate::dialect::CellDialect,
    config: serde_json::Value,
    names: Vec<String>,
    decomposition: bool,
    discovery: Option<crate::plugin::RlmDiscovery>,
}

impl ListingKey {
    fn matches(
        &self,
        catalog: &ToolCatalog,
        dialect: &SessionDialect,
        features: crate::protocol::RlmPromptFeatures,
        discovery: Option<&crate::plugin::RlmDiscovery>,
        physical: &crate::dialect::CellDialect,
    ) -> bool {
        self.catalog.len() == catalog.entries().len()
            && self
                .catalog
                .iter()
                .zip(catalog.entries())
                .all(|(left, right)| {
                    left.manifest == right.manifest && left.contract == right.contract
                })
            && self.dialect.same_presentation(physical)
            && self.config == *dialect.tool_presentation()
            && self.names == dialect.reserved_names
            && self.decomposition == features.decomposition
            && self.discovery.as_ref() == discovery
    }
}

impl ToolListing {
    /// The catalogue as the session's tool presenter lists it. Without
    /// `discovery`, every member. With it, the discovery tool and the
    /// control tools, then what fits the recorded budget. The presenter
    /// words the listing; which tools are offered, and under which call
    /// paths, is this mode's, and a listing that does not account for
    /// exactly the offered tools is refused.
    pub(crate) fn of(
        tool_catalog: &ToolCatalog,
        dialect: &crate::dialect::SessionDialect,
        features: crate::protocol::RlmPromptFeatures,
        discovery: Option<&crate::plugin::RlmDiscovery>,
    ) -> Result<Self, PresentationRefusal> {
        Self::of_with(tool_catalog, dialect, features, discovery, || {})
    }

    fn of_with(
        tool_catalog: &ToolCatalog,
        dialect: &SessionDialect,
        features: crate::protocol::RlmPromptFeatures,
        discovery: Option<&crate::plugin::RlmDiscovery>,
        after_render: impl FnOnce(),
    ) -> Result<Self, PresentationRefusal> {
        // Only the shipped pure presenters opt in. Replacing a presenter
        // preserves every host callback and its live behavior.
        let Some(physical) = dialect.cached_presentation() else {
            return Self::render(tool_catalog, dialect, features, discovery);
        };
        static CACHE: Listings = Listings(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let cache = &CACHE;
        let find = |entries: &std::collections::VecDeque<(ListingKey, ToolListing)>| {
            entries
                .iter()
                .find(|(key, _)| key.matches(tool_catalog, dialect, features, discovery, &physical))
                .map(|(_, listing)| listing.clone())
        };
        if let Some(listing) = find(
            &cache
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        ) {
            return Ok(listing);
        }
        let listing = Self::render(tool_catalog, dialect, features, discovery)?;
        after_render();
        let mut entries = cache
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = find(&entries) {
            return Ok(existing);
        }
        if entries.len() == 8 {
            entries.pop_front();
        }
        entries.push_back((
            ListingKey {
                catalog: tool_catalog.entries().to_vec(),
                dialect: physical,
                config: dialect.tool_presentation().clone(),
                names: dialect.reserved_names.clone(),
                decomposition: features.decomposition,
                discovery: discovery.cloned(),
            },
            listing.clone(),
        ));
        Ok(listing)
    }

    fn render(
        tool_catalog: &ToolCatalog,
        dialect: &SessionDialect,
        features: crate::protocol::RlmPromptFeatures,
        discovery: Option<&crate::plugin::RlmDiscovery>,
    ) -> Result<Self, PresentationRefusal> {
        let offered = tool_catalog
            .entries()
            .iter()
            .filter(|tool| {
                features.decomposition
                    || tool.manifest.tool() != crate::control_tools::CONTINUE_AS_OPERATION
            })
            .map(|tool| (tool, dialect.tool_call_path(&tool.manifest)))
            .collect::<Vec<_>>();
        let entries = offered
            .iter()
            .map(|(tool, call_path)| ToolPresentationEntry {
                manifest: &tool.manifest,
                contract: &tool.contract,
                call_path,
                pinned: discovery
                    .is_some_and(|discovery| tool.manifest.tool() == discovery.operation)
                    || !tool.manifest.declaration().controls.is_empty(),
            })
            .collect::<Vec<_>>();
        let request = ToolPresentationRequest {
            reserved_names: &dialect.reserved_names,
            max_section_bytes: None,
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
        let section = presenter.present(request)?;
        if !section.accounts_for(&request) {
            return Err(PresentationRefusal::Listing {
                presenter: presenter.id().to_owned(),
            });
        }
        let names = |ids: &[lash_vocabulary::tool_contract::ToolId]| {
            entries
                .iter()
                .filter(|entry| ids.contains(&entry.manifest.id))
                .map(|entry| entry.manifest.tool())
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

pub(crate) fn section_refused(presenter: &str, refusal: PresentationRefusal) -> SessionError {
    SessionError::Plugin(
        lash_execution_contracts::plugin_error::PluginError::ToolPresentationRefused {
            plugin_id: crate::plugin::RLM_PROTOCOL_PLUGIN_ID.to_owned(),
            presenter: presenter.to_owned(),
            code: refusal.code().to_owned(),
            message: refusal.to_string(),
        },
    )
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

fn presentation_refused(refusal: PresentationRefusal) -> crate::dialect::DialectRefusal {
    crate::dialect::DialectRefusal {
        message: refusal.to_string(),
        kind: crate::dialect::DialectRefusalKind::PresentationRefused(refusal),
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
/// itself.
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

/// A discovered tool's signature from its operation and admitted schemas in
/// the session's `dialect` and `tool_presentation`: the complete bundle of
/// named definitions and the final signature line, without Markdown or tool
/// prose. Retain the complete bundle so every reference has its definition.
///
/// # Errors
///
/// The dialect's tool presenter refuses.
pub fn discovered_tool_signature(
    dialect: &crate::dialect::CellDialect,
    tool_presentation: &serde_json::Value,
    operation: &lash_kernel_value::names::QualifiedOperation,
    input: &lash_kernel_value::json_schema::JsonSchema,
    output: &lash_kernel_value::json_schema::JsonSchema,
) -> Result<String, crate::dialect::DialectRefusal> {
    let call_path = lash_kernel_dialect::spell_operation(dialect.prompts().spelling(), operation);
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

/// A loaded tool's one-line signature, without Markdown or field notes.
/// This is the same signature [`discovered_tool_doc`] puts in its first line.
/// For a binding and schemas without a loaded definition, use
/// [`discovered_tool_signature`].
///
/// ```rust
/// use lash::rlm::{CellDialect, loaded_tool_signature};
/// use lash::tools::ToolDefinition;
/// use serde_json::json;
///
/// let tool = ToolDefinition::raw(
///     "lookup", "tools.lookup", "Find a record.",
///     json!({
///         "type": "object",
///         "properties": { "query": { "type": "string" } },
///         "required": ["query"],
///         "additionalProperties": false
///     }),
///     json!({ "type": "string" }),
/// )?
/// .with_execution(std::time::Duration::from_secs(30));
/// let signature = loaded_tool_signature(&CellDialect::typescript(), &json!({}), &tool)?;
/// assert_eq!(signature, "tools.lookup({ query: string }, options?: { signal?: AbortSignal }): Promise<string>");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
///
/// The dialect's tool presenter refuses.
pub fn loaded_tool_signature(
    dialect: &crate::dialect::CellDialect,
    tool_presentation: &serde_json::Value,
    tool: &lash_vocabulary::tool_contract::ToolDefinition,
) -> Result<String, crate::dialect::DialectRefusal> {
    discovered_tool_signature(
        dialect,
        tool_presentation,
        &tool.manifest.operation,
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
/// The dialect's tool presenter refuses.
pub fn discovered_tool_doc(
    dialect: &crate::dialect::CellDialect,
    tool_presentation: &serde_json::Value,
    tool: &lash_vocabulary::tool_contract::ToolDefinition,
) -> Result<String, crate::dialect::DialectRefusal> {
    let call_path = lash_kernel_dialect::spell_operation(
        dialect.prompts().spelling(),
        &tool.manifest.operation,
    );
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
    tool: &lash_vocabulary::tool_contract::ToolDefinition,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::typescript_test_dialect;
    use lash_core::facade_support::build_tool_catalog;
    use lash_core::test_support::ToolCatalogBuildInput;
    use lash_vocabulary::tool_contract::ToolDefinition;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn native_rlm_and_validation_share_one_pinned_definition_under_registry_drift() {
        let definition = ToolDefinition::raw(
            "tool:test/pinned",
            "authority.pinned",
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
        .with_execution(std::time::Duration::from_secs(120));
        let drifted = ToolDefinition::raw(
            "tool:test/pinned",
            "authority.pinned",
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
        .with_execution(std::time::Duration::from_secs(120));
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
    }

    /// FIG-4544. An MCP server hands its schemas through as written, and
    /// almost none of them say `additionalProperties: false`. The tool docs
    /// must still show every field's name and type, nested ones included,
    /// whether or not the field has a description.
    #[test]
    fn an_mcp_style_open_schema_renders_every_field_name_and_type() {
        let search = ToolDefinition::raw(
            "tool:mcp/issues_search",
            "tracker.issues_search",
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
        .with_execution(std::time::Duration::from_secs(120));
        let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![search]);

        let docs = rlm_prompt_tool_docs(
            &catalog,
            &typescript_test_dialect(),
            crate::protocol::RlmPromptFeatures::default(),
        );
        assert_eq!(
            docs,
            concat!(
                "`tracker.issuesSearch({ query: string; ",
                "filter?: { labels?: Array<string>; state?: \"open\" | \"closed\" }; ",
                "page?: number | null; ",
                "sort?: Array<{ field: string; descending?: boolean }> }, options?: { signal?: AbortSignal }): ",
                "Promise<{ issues: Array<{ id: number; title: string }> }>`\n",
                "[MCP tracker] Search issues.\n",
                "Parameters:\n",
                "- `input.query` (min length 1)\n",
                "- `input.page` (>= 1)\n",
                "- `input.sort` — Sort keys, most significant first.\n",
                "- `input.sort[].descending` (default false)\n",
                "Return fields:\n",
                "- `output.issues[].id` — Stable issue id."
            )
        );
    }

    fn tool_with_prose(description: &str, schema_description: &str) -> ToolDefinition {
        ToolDefinition::raw(
            "tool:test/spawn_agent",
            "agents.spawn",
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
    }

    #[test]
    fn typescript_tool_prose_is_rendered_verbatim() {
        let description = "Run a TypeScript child session using execute_code.";
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
    tools: &[lash_vocabulary::tool_contract::ToolManifest],
    discovery: Option<&crate::plugin::RlmDiscovery>,
) -> Result<(), PluginError> {
    if let Some(discovery) = discovery
        && !tools
            .iter()
            .any(|tool| tool.inline && tool.tool() == discovery.operation)
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
    #[test]
    fn discovery_filters_each_dialect_and_channel_and_requires_an_inline_operation() {
        let tool = |name: &str, inline| {
            let mut tool = lash_vocabulary::tool_contract::ToolDefinition::raw(
                name,
                format!("tools.{name}"),
                format!("Description for {name}"),
                serde_json::json!({"type":"object"}),
                serde_json::json!({"type":"string"}),
            )
            .expect("valid declared tool schemas")
            .with_execution(std::time::Duration::from_secs(120));
            tool.manifest.inline = inline;
            tool
        };
        let catalog = ToolCatalog::from_tool_definitions(vec![
            tool("search", true),
            tool("hidden", false),
            tool("visible", true),
        ]);
        let manifests = catalog
            .entries()
            .iter()
            .map(|entry| entry.manifest.clone())
            .collect::<Vec<_>>();
        {
            let dialect = crate::dialect::typescript_test_dialect();
            {
                for discovery in [
                    None,
                    Some(crate::plugin::RlmDiscovery {
                        operation: "tools.search".into(),
                        catalogue_bytes: usize::MAX,
                    }),
                ] {
                    validate_discovery(&manifests, discovery.as_ref()).unwrap();
                    let visible = catalog.clone();
                    let execution = dialect
                        .render_execution_section(Default::default(), &visible, discovery.as_ref())
                        .expect("the catalogue presents");
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
