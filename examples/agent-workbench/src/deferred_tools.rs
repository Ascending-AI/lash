//! Production deferred-tool policy for the Agent Workbench.
//!
//! The resident `tools.search` capability searches a compact catalogue and
//! persists every returned grant in SQLite. The workbench's deferred
//! provider resolves only those persisted paths, each to a match whose
//! execution binding names its call path, and executes them. Because Lash
//! VM gathers call paths before it starts a block, a path discovered by
//! `tools.search` is first callable in the next block; the SQLite grant
//! survives later turns and process restarts, and a committed match is
//! never resolved again.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lash::tools::{
    CataloguePreviewOptions, DeferredOutcome, DeferredResolveContext, DeferredToolProvider,
    ExecutionBinding, StaticToolExecute, StaticToolProvider, ToolAttemptOutcome, ToolCall,
    ToolContract, ToolDefinition, ToolFailure, ToolId, ToolManifest, ToolOutcome, ToolProvider,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const SEARCH_CALL_PATH: &str = "tools.search";
/// The key the workbench declares its deferred provider under.
pub(crate) const DEFERRED_PROVIDER_KEY: lash::plugins::ToolProviderKey =
    lash::tool_provider_key!("deferred");
/// The execution binding format the deferred provider writes and reads:
/// `{"kind": "agent_workbench_deferred", "call_path": <path>}`.
/// version_surface = "coexist"
/// version_guard(items(BINDING_FORMAT, binding_for))
pub(crate) const BINDING_FORMAT: lash::plugins::FormatVersion = lash::plugins::FormatVersion::ONE;

/// The provider every deferred match executes through: the workbench
/// plugin's deferred provider.
fn deferred_provider() -> lash::plugins::ToolProviderHandle {
    lash::plugins::ToolProviderHandle::new(
        lash::plugins::RegistryKind::Session,
        "agent_workbench",
        DEFERRED_PROVIDER_KEY,
    )
}

/// The execution binding of the match for `call_path`.
fn binding_for(call_path: &str) -> ExecutionBinding {
    ExecutionBinding {
        codec: deferred_provider(),
        format: BINDING_FORMAT,
        payload: json!({
            "kind": "agent_workbench_deferred",
            "call_path": call_path,
        }),
    }
}
const PREVIEW_MODULE_LIMIT: usize = 2;
const PREVIEW_CALL_NAME_LIMIT: usize = 4;
const SEARCH_RESULT_LIMIT: usize = 20;

#[derive(Clone)]
pub(crate) struct WorkbenchDeferredTools {
    catalogue: Arc<DeferredCatalogue>,
    store: DeferredGrantStore,
    provider: Arc<WorkbenchDeferredProvider>,
}

impl WorkbenchDeferredTools {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, rusqlite::Error> {
        Self::with_store(DeferredGrantStore::open(path)?)
    }

    pub(crate) fn in_memory() -> Result<Self, rusqlite::Error> {
        Self::with_store(DeferredGrantStore::in_memory()?)
    }

    fn with_store(store: DeferredGrantStore) -> Result<Self, rusqlite::Error> {
        let catalogue = Arc::new(DeferredCatalogue::new(workbench_deferred_definitions()));
        let provider = Arc::new(WorkbenchDeferredProvider {
            catalogue: Arc::clone(&catalogue),
            store: store.clone(),
        });
        Ok(Self {
            catalogue,
            store,
            provider,
        })
    }

    pub(crate) fn search_provider(&self) -> Arc<dyn ToolProvider> {
        Arc::new(StaticToolProvider::new(
            vec![search_tool_definition()],
            SearchDeferredTools {
                catalogue: Arc::clone(&self.catalogue),
                store: self.store.clone(),
            },
        ))
    }

    /// The session's deferred provider: it resolves the granted paths and
    /// executes their matches.
    pub(crate) fn deferred_provider(&self) -> Arc<dyn DeferredToolProvider> {
        self.provider.clone()
    }
}

pub(crate) fn prompt_preview() -> String {
    DeferredCatalogue::new(workbench_deferred_definitions()).preview()
}

/// The call paths `tools.search` granted, persisted.
#[derive(Clone)]
struct DeferredGrantStore {
    connection: Arc<Mutex<Connection>>,
}

impl DeferredGrantStore {
    fn open(path: impl AsRef<Path>) -> Result<Self, rusqlite::Error> {
        let connection = Connection::open(path)?;
        Self::initialize(connection)
    }

    fn in_memory() -> Result<Self, rusqlite::Error> {
        Self::initialize(Connection::open_in_memory()?)
    }

    fn initialize(connection: Connection) -> Result<Self, rusqlite::Error> {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS deferred_tool_grants (\
                call_path TEXT PRIMARY KEY NOT NULL\
            );",
        )?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    fn grant(&self, call_path: &str) -> Result<(), GrantStoreError> {
        self.connection
            .lock()
            .map_err(|_| GrantStoreError::Poisoned)?
            .execute(
                "INSERT INTO deferred_tool_grants (call_path) VALUES (?1) \
                 ON CONFLICT(call_path) DO NOTHING",
                params![call_path],
            )?;
        Ok(())
    }

    fn granted(&self, call_path: &str) -> Result<bool, GrantStoreError> {
        Ok(self
            .connection
            .lock()
            .map_err(|_| GrantStoreError::Poisoned)?
            .query_row(
                "SELECT 1 FROM deferred_tool_grants WHERE call_path = ?1",
                [call_path],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
}

#[derive(Debug, thiserror::Error)]
enum GrantStoreError {
    #[error("deferred-package database lock is poisoned")]
    Poisoned,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// The workbench's deferred provider. It advertises nothing; it resolves a
/// granted call path to its catalogued tool, executed by itself under a
/// binding naming the path, and runs that tool's calls.
struct WorkbenchDeferredProvider {
    catalogue: Arc<DeferredCatalogue>,
    store: DeferredGrantStore,
}

impl WorkbenchDeferredProvider {
    fn definition_by_id(&self, id: &ToolId) -> Option<&ToolDefinition> {
        self.catalogue
            .definitions
            .values()
            .find(|definition| definition.id() == id)
    }

    fn outcome(&self, path: &str) -> DeferredOutcome {
        let Some(definition) = self.catalogue.definitions.get(path) else {
            return DeferredOutcome::Unavailable { note: None };
        };
        match self.store.granted(path) {
            Ok(true) => DeferredOutcome::resolved(
                deferred_provider(),
                definition.clone(),
                Some(binding_for(path)),
            ),
            Ok(false) => DeferredOutcome::Unavailable { note: None },
            Err(err) => {
                // A resolution has no error channel; surface the store
                // failure to the operator before recording the path as
                // unavailable (a re-search will retry in a later run).
                eprintln!(
                    "warning: deferred grant store read failed for `{path}` \
                     ({err}); resolving as not available"
                );
                DeferredOutcome::Unavailable { note: None }
            }
        }
    }
}

#[async_trait]
impl DeferredToolProvider for WorkbenchDeferredProvider {
    fn searchable_names(&self, _: &DeferredResolveContext<'_>) -> BTreeMap<String, String> {
        self.catalogue
            .definitions
            .iter()
            .map(|(path, definition)| (definition.manifest.tool(), path.clone()))
            .collect()
    }
    async fn resolve(
        &self,
        _cx: &DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> BTreeMap<String, DeferredOutcome> {
        paths
            .iter()
            .map(|path| ((*path).to_string(), self.outcome(path)))
            .collect()
    }
}

#[async_trait]
impl ToolProvider for WorkbenchDeferredProvider {
    fn tool_manifests(
        &self,
        _config: &lash::tools::OwnerConfigView,
    ) -> std::result::Result<Vec<ToolManifest>, lash::plugins::PluginError> {
        Ok(Vec::new())
    }

    fn resolve_manifest_by_id(
        &self,
        id: &ToolId,
        _config: &lash::tools::OwnerConfigView,
    ) -> Option<ToolManifest> {
        self.definition_by_id(id).map(ToolDefinition::manifest)
    }

    fn resolve_contract(
        &self,
        _name: &str,
        _config: &lash::tools::OwnerConfigView,
    ) -> Option<Arc<ToolContract>> {
        None
    }

    fn resolve_contract_by_id(
        &self,
        id: &ToolId,
        _config: &lash::tools::OwnerConfigView,
    ) -> Option<Arc<ToolContract>> {
        self.definition_by_id(id)
            .map(|definition| Arc::new(definition.contract()))
    }

    fn readable_binding_formats(&self) -> &'static [lash::plugins::FormatVersion] {
        &[BINDING_FORMAT]
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        let Some(definition) = self
            .catalogue
            .definitions
            .values()
            .find(|definition| definition.manifest.tool() == call.name())
        else {
            return ToolOutcome::err_fmt(format_args!("unknown deferred tool `{}`", call.name()))
                .into();
        };
        let call_path = deferred_call_path(definition);
        let bound = call
            .context
            .tool_execution_binding()
            .map(|binding| &binding.payload["call_path"]);
        if bound != Some(&json!(call_path)) {
            return ToolOutcome::failure(ToolFailure::new(
                "invalid_deferred_grant",
                "execution binding does not match the deferred tool",
            ))
            .into();
        }
        execute_workbench_utility(&call_path, call.args).into()
    }
}

struct DeferredCatalogue {
    definitions: BTreeMap<String, ToolDefinition>,
}

impl DeferredCatalogue {
    #[expect(
        clippy::expect_used,
        reason = "workbench_deferred_definitions() always returns at least one tool, so the \
                  preview builder's empty-catalogue refusal cannot fire"
    )]
    fn preview(&self) -> String {
        lash::tools::catalogue_preview(
            lash::tools::catalogue_preview_entries_from_manifests(
                self.definitions
                    .values()
                    .map(ToolDefinition::manifest)
                    .collect::<Vec<_>>()
                    .iter(),
            ),
            &CataloguePreviewOptions {
                title: "Deferred workbench utilities".to_string(),
                search_call_path: SEARCH_CALL_PATH.to_string(),
                module_limit: PREVIEW_MODULE_LIMIT,
                call_name_limit: PREVIEW_CALL_NAME_LIMIT,
                ..CataloguePreviewOptions::default()
            },
        )
        .expect("workbench deferred catalogue is not empty")
    }

    fn new(definitions: Vec<ToolDefinition>) -> Self {
        let definitions = definitions
            .into_iter()
            .map(|definition| {
                let call_path = deferred_call_path(&definition);
                (call_path, definition)
            })
            .collect();
        Self { definitions }
    }

    fn search(&self, query: &str, limit: usize) -> Vec<DeferredSearchResult> {
        let terms = query
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>();
        let mut ranked = self
            .definitions
            .iter()
            .filter_map(|(call_path, definition)| {
                let haystack = format!(
                    "{} {} {}",
                    call_path,
                    definition.manifest.tool(),
                    definition.description()
                )
                .to_ascii_lowercase();
                let score = terms
                    .iter()
                    .filter(|term| haystack.contains(term.as_str()))
                    .count();
                (score > 0).then_some((score, call_path, definition))
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(right.1)));
        ranked
            .into_iter()
            .take(limit.min(SEARCH_RESULT_LIMIT))
            .map(|(_, call_path, definition)| DeferredSearchResult::new(call_path, definition))
            .collect()
    }
}

#[derive(serde::Serialize)]
struct DeferredSearchResult {
    call_path: String,
    signature: String,
    description: String,
}

impl DeferredSearchResult {
    /// A catalogued tool as the session's dialect shows it: the same
    /// signature its resident tool docs would carry.
    #[expect(
        clippy::expect_used,
        reason = "the fixed deferred tool schemas are pinned by the discovery laws"
    )]
    fn new(call_path: &str, definition: &ToolDefinition) -> Self {
        use lash::rlm::{CellDialect, discovered_tool_signature};
        let contract = definition.contract();
        let input = &contract.input_schema.canonical;
        Self {
            call_path: call_path.to_string(),
            signature: discovered_tool_signature(
                &CellDialect::typescript(),
                &json!({}),
                definition.operation(),
                input,
                &contract.output_schema.canonical,
            )
            .expect("built-in deferred tool schemas are representable"),
            description: definition.description().trim().to_string(),
        }
    }
}

struct SearchDeferredTools {
    catalogue: Arc<DeferredCatalogue>,
    store: DeferredGrantStore,
}

#[async_trait]
impl StaticToolExecute for SearchDeferredTools {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        (async {
            let query = call
                .args
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if query.trim().is_empty() {
                return ToolOutcome::failure(ToolFailure::new(
                    "invalid_query",
                    "tools.search requires a non-empty query",
                ));
            }
            let limit = call
                .args
                .get("limit")
                .and_then(Value::as_u64)
                .and_then(|limit| usize::try_from(limit).ok())
                .unwrap_or(5)
                .clamp(1, SEARCH_RESULT_LIMIT);
            let results = self.catalogue.search(query, limit);
            for result in &results {
                if let Err(error) = self.store.grant(&result.call_path) {
                    return ToolOutcome::failure(ToolFailure::new(
                        "grant_persistence_failed",
                        error.to_string(),
                    ));
                }
            }
            ToolOutcome::ok(json!({ "results": results }))
        })
        .await
        .into()
    }
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
fn search_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "workbench:search_tools",
        "tools.search",
        "Search the deferred workbench utility catalogue by capability. Discovered operations become callable in your next code block.",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "minLength": 1 },
                "limit": { "type": "integer", "minimum": 1, "maximum": SEARCH_RESULT_LIMIT, "default": 5 }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        json!({
            "type": "object",
            "properties": {
                "results": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "call_path": { "type": "string" },
                            "signature": { "type": "string" },
                            "description": { "type": "string" }
                        },
                        "required": ["call_path", "signature", "description"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["results"],
            "additionalProperties": false
        }),
    ).expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

/// The path a TypeScript cell calls `definition` by.
fn deferred_call_path(definition: &ToolDefinition) -> String {
    lash::dialect::spell_operation(lash::dialect::Spelling::TypeScript, definition.operation())
}

fn workbench_deferred_definitions() -> Vec<ToolDefinition> {
    vec![
        utility_definition(
            "strings_stats",
            ["strings"],
            "stats",
            "Count bytes, Unicode characters, whitespace-delimited words, and lines in text.",
            json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
                "additionalProperties": false
            }),
            json!({
                "type": "object",
                "properties": {
                    "bytes": { "type": "integer" },
                    "characters": { "type": "integer" },
                    "words": { "type": "integer" },
                    "lines": { "type": "integer" }
                },
                "required": ["bytes", "characters", "words", "lines"],
                "additionalProperties": false
            }),
        ),
        utility_definition(
            "strings_sha256",
            ["strings"],
            "sha256",
            "Compute the lowercase SHA-256 checksum of UTF-8 text for exact content comparisons.",
            one_field_schema("text", "string"),
            one_field_schema("digest", "string"),
        ),
        utility_definition(
            "objects_keys",
            ["objects"],
            "keys",
            "List an object's keys in deterministic lexical order.",
            one_field_schema("value", "object"),
            string_list_field_schema("keys"),
        ),
        utility_definition(
            "objects_pointer",
            ["objects"],
            "pointer",
            "Resolve an RFC 6901 JSON Pointer and report whether it exists.",
            json!({
                "type": "object",
                "properties": {
                    "value": {},
                    "pointer": { "type": "string" }
                },
                "required": ["value", "pointer"],
                "additionalProperties": false
            }),
            json!({
                "type": "object",
                "properties": { "found": { "type": "boolean" }, "value": {} },
                "required": ["found", "value"],
                "additionalProperties": false
            }),
        ),
        utility_definition(
            "lists_sort",
            ["lists"],
            "sort",
            "Sort strings lexically with an optional descending order.",
            json!({
                "type": "object",
                "properties": {
                    "values": { "type": "array", "items": { "type": "string" } },
                    "descending": { "type": "boolean", "default": false }
                },
                "required": ["values"],
                "additionalProperties": false
            }),
            string_list_field_schema("values"),
        ),
        utility_definition(
            "lists_unique",
            ["lists"],
            "unique",
            "Remove duplicate strings while preserving their first-seen order.",
            string_list_field_schema("values"),
            string_list_field_schema("values"),
        ),
    ]
}

#[expect(clippy::expect_used, reason = "this fixture declares valid schemas")]
fn utility_definition(
    name: &str,
    module: [&str; 1],
    operation: &str,
    description: &str,
    input_schema: Value,
    output_schema: Value,
) -> ToolDefinition {
    ToolDefinition::raw(
        format!("workbench:deferred:{name}"),
        format!("{}.{operation}", module.join(".")),
        description,
        input_schema,
        output_schema,
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

fn one_field_schema(field: &str, ty: &str) -> Value {
    json!({
        "type": "object",
        "properties": { (field): { "type": ty } },
        "required": [field],
        "additionalProperties": false
    })
}

fn string_list_field_schema(field: &str) -> Value {
    json!({
        "type": "object",
        "properties": { (field): { "type": "array", "items": { "type": "string" } } },
        "required": [field],
        "additionalProperties": false
    })
}

fn execute_workbench_utility(call_path: &str, args: &Value) -> ToolOutcome {
    match call_path {
        "strings.stats" => {
            let Some(text) = args.get("text").and_then(Value::as_str) else {
                return invalid_arguments("strings.stats requires string `text`");
            };
            ToolOutcome::ok(json!({
                "bytes": text.len(),
                "characters": text.chars().count(),
                "words": text.split_whitespace().count(),
                "lines": text.lines().count(),
            }))
        }
        "strings.sha256" => {
            let Some(text) = args.get("text").and_then(Value::as_str) else {
                return invalid_arguments("strings.sha256 requires string `text`");
            };
            ToolOutcome::ok(json!({ "digest": format!("{:x}", Sha256::digest(text.as_bytes())) }))
        }
        "objects.keys" => {
            let Some(object) = args.get("value").and_then(Value::as_object) else {
                return invalid_arguments("objects.keys requires object `value`");
            };
            ToolOutcome::ok(json!({ "keys": object.keys().collect::<Vec<_>>() }))
        }
        "objects.pointer" => {
            let Some(pointer) = args.get("pointer").and_then(Value::as_str) else {
                return invalid_arguments("objects.pointer requires string `pointer`");
            };
            let found = args.get("value").and_then(|value| value.pointer(pointer));
            ToolOutcome::ok(json!({
                "found": found.is_some(),
                "value": found.cloned().unwrap_or(Value::Null),
            }))
        }
        "lists.sort" => {
            let Some(values) = string_values(args) else {
                return invalid_arguments("lists.sort requires string array `values`");
            };
            let mut values = values;
            values.sort();
            if args.get("descending").and_then(Value::as_bool) == Some(true) {
                values.reverse();
            }
            ToolOutcome::ok(json!({ "values": values }))
        }
        "lists.unique" => {
            let Some(values) = string_values(args) else {
                return invalid_arguments("lists.unique requires string array `values`");
            };
            let mut seen = BTreeSet::new();
            let values = values
                .into_iter()
                .filter(|value| seen.insert(value.clone()))
                .collect::<Vec<_>>();
            ToolOutcome::ok(json!({ "values": values }))
        }
        _ => ToolOutcome::failure(ToolFailure::new(
            "unknown_deferred_tool",
            format!("unknown deferred call path `{call_path}`"),
        )),
    }
}

fn string_values(args: &Value) -> Option<Vec<String>> {
    args.get("values")?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect()
}

fn invalid_arguments(message: &str) -> ToolOutcome {
    ToolOutcome::failure(ToolFailure::new("invalid_arguments", message))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-6490: discovery publishes only the callable contract; neither the
    /// result schema nor its actual records carry authored tool examples.
    #[test]
    fn discovery_has_no_authored_examples_and_refuses_the_legacy_field() {
        let definition = search_tool_definition();
        let contract = definition.contract();
        let schema = contract.output_schema.canonical.as_value();
        let item = &schema["properties"]["results"]["items"];
        assert!(item["properties"].get("examples").is_none());
        assert!(
            !item["required"]
                .as_array()
                .expect("required fields")
                .contains(&json!("examples"))
        );
        let catalogue = DeferredCatalogue::new(workbench_deferred_definitions());
        let results = catalogue.search("text checksum", 3);
        assert!(!results.is_empty());
        let mut wire = json!({ "results": results });
        for result in wire["results"].as_array().expect("results") {
            assert!(result.get("examples").is_none());
        }
        contract
            .output_schema
            .canonical
            .validate(&wire)
            .expect("discovery matches its schema");
        wire["results"][0]["examples"] = json!([]);
        contract
            .output_schema
            .canonical
            .validate(&wire)
            .expect_err("legacy authored examples are refused");
        assert!(!prompt_preview().to_lowercase().contains("examples"));
    }

    /// Resolve `paths` the way a process cell's link asks: no run, no
    /// capability refs. The workbench grants by call path alone.
    async fn resolve(
        provider: &WorkbenchDeferredProvider,
        paths: &[&str],
    ) -> BTreeMap<String, DeferredOutcome> {
        let owner = lash::tools::ExecutionOwner::Process {
            process_id: lash::ProcessId::fixture("p_deferred"),
        };
        let capabilities = BTreeMap::new();
        let config = lash::tools::OwnerConfigView::default();
        provider
            .resolve(
                &DeferredResolveContext::new(
                    &deferred_provider(),
                    &owner,
                    None,
                    &capabilities,
                    &config,
                ),
                paths,
            )
            .await
    }

    #[tokio::test]
    async fn grants_persist_across_turn_boundaries_and_sqlite_reopen() {
        let temp = tempfile::tempdir().expect("deferred grant tempdir");
        let path = temp.path().join("grants.db");
        let first = WorkbenchDeferredTools::open(&path).expect("open first grant store");
        first
            .store
            .grant("strings.sha256")
            .expect("persist deferred grant");

        let turn_two = resolve(&first.provider, &["strings.sha256"]).await;
        assert!(matches!(
            turn_two.get("strings.sha256"),
            Some(DeferredOutcome::Resolved { .. })
        ));
        drop(first);

        let reopened = WorkbenchDeferredTools::open(&path).expect("reopen SQLite grant store");
        let after_restart = resolve(&reopened.provider, &["strings.sha256"]).await;
        let Some(DeferredOutcome::Resolved { link }) = after_restart.get("strings.sha256") else {
            panic!("expected the restored grant, got {after_restart:?}");
        };
        assert_eq!(link.tool.as_str(), "workbench:deferred:strings_sha256");
        assert_eq!(link.provider, deferred_provider());
        assert_eq!(link.payload.as_ref(), Some(&binding_for("strings.sha256")));
    }

    #[tokio::test]
    async fn unavailable_resolution_is_typed_and_deterministic() {
        let tools = WorkbenchDeferredTools::in_memory().expect("open deferred tools");
        let outcomes = resolve(&tools.provider, &["strings.sha256", "unknown.path"]).await;
        assert!(matches!(
            outcomes.get("strings.sha256"),
            Some(DeferredOutcome::Unavailable { note: None })
        ));
        assert!(matches!(
            outcomes.get("unknown.path"),
            Some(DeferredOutcome::Unavailable { note: None })
        ));
        assert_eq!(
            outcomes.keys().cloned().collect::<Vec<_>>(),
            vec!["strings.sha256".to_string(), "unknown.path".to_string()]
        );
    }

    #[test]
    fn search_ranking_and_preview_truncation_are_deterministic() {
        let tools = WorkbenchDeferredTools::in_memory().expect("open deferred tools");
        let first = tools.catalogue.search("text checksum", 3);
        let second = tools.catalogue.search("text checksum", 3);
        assert_eq!(
            first
                .iter()
                .map(|result| result.call_path.as_str())
                .collect::<Vec<_>>(),
            vec!["strings.sha256", "strings.stats"]
        );
        assert_eq!(
            first
                .iter()
                .map(|result| result.call_path.as_str())
                .collect::<Vec<_>>(),
            second
                .iter()
                .map(|result| result.call_path.as_str())
                .collect::<Vec<_>>()
        );

        let preview = prompt_preview();
        let repeated = prompt_preview();
        assert_eq!(preview, repeated);
        assert!(preview.contains("Modules: 3 total"), "{preview}");
        assert!(!preview.contains("Catalogued calls:"));
        assert!(preview.contains(SEARCH_CALL_PATH), "{preview}");
    }
}
