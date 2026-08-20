//! Production deferred-tool policy for the Agent Workbench.
//!
//! The resident `tools.search` capability searches a compact catalogue and
//! persists every returned grant in SQLite. The RLM resolver authorizes only
//! those persisted paths. Because Lashlang gathers call paths before it starts
//! a block, a path discovered by `tools.search` is first callable in the next
//! block; the SQLite grant survives later turns and process restarts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lash::sync::MutexExt;
use lash::tools::{
    CataloguePreviewOptions, DeferredToolGrant, DeferredToolResolution, DeferredToolResolver,
    SharedDeferredToolResolver, StaticToolExecute, StaticToolProvider, ToolBinding, ToolCall,
    ToolContract, ToolDefinition, ToolDefinitionBindingExt, ToolExecutionGrant, ToolId,
    ToolManifest, ToolManifestBindingExt, ToolOutcome, ToolPrepareCall, ToolProvider,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const SEARCH_TOOL_NAME: &str = "search_tools";
const SEARCH_CALL_PATH: &str = "tools.search";
const DEFERRED_SOURCE_ID: &str = lash::tools::PLUGIN_TOOL_SOURCE_ID;
const PREVIEW_MODULE_LIMIT: usize = 2;
const PREVIEW_CALL_NAME_LIMIT: usize = 4;
const SEARCH_RESULT_LIMIT: usize = 20;

#[derive(Clone)]
pub(crate) struct WorkbenchDeferredTools {
    catalogue: Arc<DeferredCatalogue>,
    store: DeferredGrantStore,
    resolver: Arc<WorkbenchDeferredToolResolver>,
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
        let resolver = Arc::new(WorkbenchDeferredToolResolver {
            store: store.clone(),
            installed: Mutex::new(BTreeMap::new()),
        });
        Ok(Self {
            catalogue,
            store,
            resolver,
        })
    }

    pub(crate) fn resolver(&self) -> SharedDeferredToolResolver {
        self.resolver.clone()
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

    pub(crate) fn execution_provider(&self) -> Arc<dyn ToolProvider> {
        Arc::new(DeferredExecutionProvider {
            definitions: self.catalogue.definitions.clone(),
        })
    }

    pub(crate) fn preview_contribution(&self) -> lash::prompt::PromptContribution {
        lash::tools::catalogue_preview_contribution_for_entries_with_options(
            lash::tools::catalogue_preview_entries_from_manifests(
                self.catalogue
                    .definitions
                    .values()
                    .map(ToolDefinition::manifest)
                    .collect::<Vec<_>>()
                    .iter(),
            ),
            CataloguePreviewOptions {
                title: "Deferred workbench utilities".to_string(),
                search_tool_name: SEARCH_TOOL_NAME.to_string(),
                search_call_path: SEARCH_CALL_PATH.to_string(),
                module_limit: PREVIEW_MODULE_LIMIT,
                call_name_limit: PREVIEW_CALL_NAME_LIMIT,
            },
        )
        .expect("workbench deferred catalogue is not empty")
    }
}

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
                call_path TEXT PRIMARY KEY NOT NULL, \
                grant_json TEXT NOT NULL\
            );",
        )?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    fn grant(&self, call_path: &str, grant: &DeferredToolGrant) -> Result<(), GrantStoreError> {
        let grant_json = serde_json::to_string(grant)?;
        self.connection
            .lock()
            .map_err(|_| GrantStoreError::Poisoned)?
            .execute(
                "INSERT INTO deferred_tool_grants (call_path, grant_json) VALUES (?1, ?2) \
                 ON CONFLICT(call_path) DO UPDATE SET grant_json = excluded.grant_json",
                params![call_path, grant_json],
            )?;
        Ok(())
    }

    fn load(&self, call_path: &str) -> Result<Option<DeferredToolGrant>, GrantStoreError> {
        let grant_json = self
            .connection
            .lock()
            .map_err(|_| GrantStoreError::Poisoned)?
            .query_row(
                "SELECT grant_json FROM deferred_tool_grants WHERE call_path = ?1",
                [call_path],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        grant_json
            .map(|raw| serde_json::from_str(&raw).map_err(GrantStoreError::from))
            .transpose()
    }
}

#[derive(Debug, thiserror::Error)]
enum GrantStoreError {
    #[error("deferred-tool grant database lock is poisoned")]
    Poisoned,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

struct WorkbenchDeferredToolResolver {
    store: DeferredGrantStore,
    installed: Mutex<BTreeMap<String, DeferredToolGrant>>,
}

#[async_trait]
impl DeferredToolResolver for WorkbenchDeferredToolResolver {
    async fn resolve(&self, paths: &[&str]) -> BTreeMap<String, DeferredToolResolution> {
        paths
            .iter()
            .map(|path| {
                let resolution = match self.store.load(path) {
                    Ok(Some(grant)) => {
                        self.installed
                            .lock_recover()
                            .insert((*path).to_string(), grant.clone());
                        DeferredToolResolution::Resolved(Box::new(grant))
                    }
                    Ok(None) => DeferredToolResolution::NotAvailable,
                    Err(err) => {
                        // The resolver seam has no error channel; surface the
                        // store failure to the operator before reporting the
                        // link-scoped NotAvailable (a re-search will retry).
                        eprintln!(
                            "warning: deferred grant store read failed for `{path}` \
                             ({err}); resolving as not available"
                        );
                        DeferredToolResolution::NotAvailable
                    }
                };
                ((*path).to_string(), resolution)
            })
            .collect()
    }

    fn install_recorded_grant(&self, path: &str, grant: &DeferredToolGrant) {
        self.installed
            .lock_recover()
            .insert(path.to_string(), grant.clone());
    }
}

struct DeferredCatalogue {
    definitions: BTreeMap<String, ToolDefinition>,
}

impl DeferredCatalogue {
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
                    definition.name(),
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
    examples: Vec<String>,
}

impl DeferredSearchResult {
    fn new(call_path: &str, definition: &ToolDefinition) -> Self {
        let compact = definition
            .contract()
            .compact_contract_with_signature_name_and_example_limit(
                &definition.manifest(),
                call_path,
                2,
            );
        Self {
            call_path: call_path.to_string(),
            signature: compact.signature,
            description: compact.description,
            examples: compact.examples,
        }
    }
}

struct SearchDeferredTools {
    catalogue: Arc<DeferredCatalogue>,
    store: DeferredGrantStore,
}

#[async_trait]
impl StaticToolExecute for SearchDeferredTools {
    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        let query = call
            .args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if query.trim().is_empty() {
            return ToolOutcome::err(json!({
                "kind": "invalid_query",
                "message": "tools.search requires a non-empty query"
            }));
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
            let definition = self
                .catalogue
                .definitions
                .get(&result.call_path)
                .expect("search result comes from deferred catalogue");
            let grant = DeferredToolGrant::new(definition.clone())
                .with_source_id(DEFERRED_SOURCE_ID)
                .with_execution_binding(json!({
                    "kind": "agent_workbench_deferred",
                    "call_path": result.call_path,
                }));
            if let Err(error) = self.store.grant(&result.call_path, &grant) {
                return ToolOutcome::err(json!({
                    "kind": "grant_persistence_failed",
                    "message": error.to_string()
                }));
            }
        }
        ToolOutcome::ok(json!({ "results": results }))
    }
}

struct DeferredExecutionProvider {
    definitions: BTreeMap<String, ToolDefinition>,
}

impl DeferredExecutionProvider {
    fn definition_by_id(&self, id: &ToolId) -> Option<&ToolDefinition> {
        self.definitions
            .values()
            .find(|definition| definition.id() == id)
    }

    async fn execute_definition(
        &self,
        definition: &ToolDefinition,
        args: &Value,
        context: &lash::tools::AttemptContext<'_>,
    ) -> ToolOutcome {
        let call_path = deferred_call_path(definition);
        if context.tool_execution_binding()["call_path"] != json!(call_path) {
            return ToolOutcome::err(json!({
                "kind": "invalid_deferred_grant",
                "message": "execution binding does not match the deferred tool"
            }));
        }
        execute_workbench_utility(&call_path, args)
    }
}

#[async_trait]
impl ToolProvider for DeferredExecutionProvider {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        Vec::new()
    }

    fn resolve_manifest_by_id(&self, id: &ToolId) -> Option<ToolManifest> {
        self.definition_by_id(id).map(ToolDefinition::manifest)
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<ToolContract>> {
        None
    }

    fn resolve_contract_by_id(&self, id: &ToolId) -> Option<Arc<ToolContract>> {
        self.definition_by_id(id)
            .map(|definition| Arc::new(definition.contract()))
    }

    async fn prepare_granted_tool_call(
        &self,
        _grant: &ToolExecutionGrant,
        call: ToolPrepareCall<'_>,
    ) -> Result<lash::tools::PreparedToolCall, ToolOutcome> {
        Ok(lash::tools::PreparedToolCall::identity(
            call.tool_id,
            call.pending,
        ))
    }

    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        let Some(definition) = self
            .definitions
            .values()
            .find(|definition| definition.name() == call.name)
        else {
            return ToolOutcome::err_fmt(format_args!("unknown deferred tool `{}`", call.name));
        };
        self.execute_definition(definition, call.args, call.context)
            .await
    }

    async fn execute_granted(
        &self,
        grant: &ToolExecutionGrant,
        args: &Value,
        context: &lash::tools::AttemptContext<'_>,
    ) -> ToolOutcome {
        let Some(definition) = self.definition_by_id(&grant.manifest.id) else {
            return ToolOutcome::err_fmt(format_args!(
                "unknown deferred tool id `{}`",
                grant.manifest.id
            ));
        };
        self.execute_definition(definition, args, context).await
    }
}

fn search_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "workbench:search_tools",
        SEARCH_TOOL_NAME,
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
                            "description": { "type": "string" },
                            "examples": { "type": "array", "items": { "type": "string" } }
                        },
                        "required": ["call_path", "signature", "description", "examples"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["results"],
            "additionalProperties": false
        }),
    )
    .with_examples(vec![
        "await tools.search({ query: \"text checksum\", limit: 3 })?".to_string(),
    ])
    .with_tool_binding(ToolBinding::new(["tools"], "search"))
}

fn deferred_call_path(definition: &ToolDefinition) -> String {
    definition
        .manifest()
        .tool_binding()
        .expect("deferred binding serializes")
        .expect("deferred workbench tools have Lashlang bindings")
        .executable_for(definition.name())
        .expect("deferred workbench binding is executable")
        .call_path()
}

fn workbench_deferred_definitions() -> Vec<ToolDefinition> {
    vec![
        utility_definition(
            "text_stats",
            ["text"],
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
            "text_sha256",
            ["text"],
            "sha256",
            "Compute the lowercase SHA-256 checksum of UTF-8 text for exact content comparisons.",
            one_field_schema("text", "string"),
            one_field_schema("digest", "string"),
        ),
        utility_definition(
            "json_keys",
            ["json"],
            "keys",
            "List an object's keys in deterministic lexical order.",
            one_field_schema("value", "object"),
            string_list_field_schema("keys"),
        ),
        utility_definition(
            "json_pointer",
            ["json"],
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
            "list_sort",
            ["list"],
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
            "list_unique",
            ["list"],
            "unique",
            "Remove duplicate strings while preserving their first-seen order.",
            string_list_field_schema("values"),
            string_list_field_schema("values"),
        ),
    ]
}

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
        format!("workbench_deferred_{name}"),
        description,
        input_schema,
        output_schema,
    )
    .with_examples(vec![format!(
        "await {}.{}({{ /* matching arguments */ }})?",
        module[0], operation
    )])
    .with_tool_binding(ToolBinding::new(module, operation))
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
        "text.stats" => {
            let Some(text) = args.get("text").and_then(Value::as_str) else {
                return invalid_arguments("text.stats requires string `text`");
            };
            ToolOutcome::ok(json!({
                "bytes": text.len(),
                "characters": text.chars().count(),
                "words": text.split_whitespace().count(),
                "lines": text.lines().count(),
            }))
        }
        "text.sha256" => {
            let Some(text) = args.get("text").and_then(Value::as_str) else {
                return invalid_arguments("text.sha256 requires string `text`");
            };
            ToolOutcome::ok(json!({ "digest": format!("{:x}", Sha256::digest(text.as_bytes())) }))
        }
        "json.keys" => {
            let Some(object) = args.get("value").and_then(Value::as_object) else {
                return invalid_arguments("json.keys requires object `value`");
            };
            ToolOutcome::ok(json!({ "keys": object.keys().collect::<Vec<_>>() }))
        }
        "json.pointer" => {
            let Some(pointer) = args.get("pointer").and_then(Value::as_str) else {
                return invalid_arguments("json.pointer requires string `pointer`");
            };
            let found = args.get("value").and_then(|value| value.pointer(pointer));
            ToolOutcome::ok(json!({
                "found": found.is_some(),
                "value": found.cloned().unwrap_or(Value::Null),
            }))
        }
        "list.sort" => {
            let Some(values) = string_values(args) else {
                return invalid_arguments("list.sort requires string array `values`");
            };
            let mut values = values;
            values.sort();
            if args.get("descending").and_then(Value::as_bool) == Some(true) {
                values.reverse();
            }
            ToolOutcome::ok(json!({ "values": values }))
        }
        "list.unique" => {
            let Some(values) = string_values(args) else {
                return invalid_arguments("list.unique requires string array `values`");
            };
            let mut seen = BTreeSet::new();
            let values = values
                .into_iter()
                .filter(|value| seen.insert(value.clone()))
                .collect::<Vec<_>>();
            ToolOutcome::ok(json!({ "values": values }))
        }
        _ => ToolOutcome::err(json!({
            "kind": "unknown_deferred_tool",
            "message": format!("unknown deferred call path `{call_path}`")
        })),
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
    ToolOutcome::err(json!({ "kind": "invalid_arguments", "message": message }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant_for(catalogue: &DeferredCatalogue, call_path: &str) -> DeferredToolGrant {
        DeferredToolGrant::new(
            catalogue
                .definitions
                .get(call_path)
                .expect("deferred definition")
                .clone(),
        )
        .with_source_id(DEFERRED_SOURCE_ID)
        .with_execution_binding(json!({
            "kind": "agent_workbench_deferred",
            "call_path": call_path,
        }))
    }

    #[tokio::test]
    async fn grants_persist_across_turn_boundaries_and_sqlite_reopen() {
        let temp = tempfile::tempdir().expect("deferred grant tempdir");
        let path = temp.path().join("grants.db");
        let first = WorkbenchDeferredTools::open(&path).expect("open first grant store");
        let grant = grant_for(&first.catalogue, "text.sha256");
        first
            .store
            .grant("text.sha256", &grant)
            .expect("persist deferred grant");

        let turn_two = first.resolver.resolve(&["text.sha256"]).await;
        assert!(matches!(
            turn_two.get("text.sha256"),
            Some(DeferredToolResolution::Resolved(_))
        ));
        drop(first);

        let reopened = WorkbenchDeferredTools::open(&path).expect("reopen SQLite grant store");
        let after_restart = reopened.resolver.resolve(&["text.sha256"]).await;
        let restored = match after_restart.get("text.sha256") {
            Some(DeferredToolResolution::Resolved(grant)) => grant.as_ref(),
            other => panic!("expected restored grant, got {other:?}"),
        };
        assert_eq!(
            restored.definition.id().as_str(),
            "workbench:deferred:text_sha256"
        );
        assert_eq!(restored.source_id.as_deref(), Some(DEFERRED_SOURCE_ID));

        reopened
            .resolver
            .install_recorded_grant("text.sha256", restored);
        assert_eq!(
            reopened
                .resolver
                .installed
                .lock_recover()
                .get("text.sha256")
                .expect("reinstalled recorded grant")
                .definition
                .id()
                .as_str(),
            "workbench:deferred:text_sha256"
        );
    }

    #[tokio::test]
    async fn unavailable_resolution_is_typed_and_deterministic() {
        let tools = WorkbenchDeferredTools::in_memory().expect("open deferred tools");
        let outcomes = tools
            .resolver
            .resolve(&["text.sha256", "unknown.path"])
            .await;
        assert!(matches!(
            outcomes.get("text.sha256"),
            Some(DeferredToolResolution::NotAvailable)
        ));
        assert!(matches!(
            outcomes.get("unknown.path"),
            Some(DeferredToolResolution::NotAvailable)
        ));
        assert_eq!(
            outcomes.keys().cloned().collect::<Vec<_>>(),
            vec!["text.sha256".to_string(), "unknown.path".to_string()]
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
            vec!["text.sha256", "text.stats"]
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

        let preview = tools.preview_contribution();
        let repeated = tools.preview_contribution();
        assert_eq!(preview.content, repeated.content);
        assert!(
            preview.content.contains("Modules: 3 total"),
            "{}",
            preview.content
        );
        assert!(!preview.content.contains("Catalogued calls:"));
        assert_eq!(preview.gate.tools, vec![SEARCH_TOOL_NAME.to_string()]);
    }

    #[test]
    fn deferred_preview_reduces_the_prompt_surface() {
        let tools = WorkbenchDeferredTools::in_memory().expect("open deferred tools");
        let resident_bytes = tools
            .catalogue
            .definitions
            .values()
            .map(|definition| {
                let call_path = deferred_call_path(definition);
                definition
                    .contract()
                    .compact_contract_with_signature_name(&definition.manifest(), &call_path)
                    .render_markdown()
                    .len()
            })
            .sum::<usize>();
        let deferred_bytes = search_tool_definition()
            .compact_contract()
            .render_markdown()
            .len()
            + tools.preview_contribution().content.len();
        assert!(
            deferred_bytes < resident_bytes,
            "deferred surface {deferred_bytes} bytes must be smaller than resident {resident_bytes} bytes"
        );
        eprintln!(
            "workbench deferred prompt bytes: resident={resident_bytes} deferred={deferred_bytes} delta={}",
            resident_bytes - deferred_bytes
        );
    }

    #[test]
    fn deferred_utility_results_are_typed() {
        let stats = execute_workbench_utility("text.stats", &json!({ "text": "one two\nthree" }));
        assert_eq!(
            stats.value_for_projection(),
            json!({ "bytes": 13, "characters": 13, "words": 3, "lines": 2 })
        );
        let digest = execute_workbench_utility("text.sha256", &json!({ "text": "restart proof" }));
        assert_eq!(
            digest.value_for_projection(),
            json!({
                "digest": "6aaa2c8b150bc016006f9d88df2e273adea1965bc4e8c66f87357b62a8e3afc9"
            })
        );
    }
}
