//! RLM-only deferred tool resolution.
//!
//! A host-provided [`DeferredToolResolver`] resolves Lashlang call-paths absent
//! from the link-time [`LashlangHostEnvironment`] into [`ToolGrant`] values
//! (which carry their Tool Execution Bindings) or reports `NotAvailable`. The
//! resolver resolves on demand only — it does not enumerate, advertise, or rank
//! tools.
//!
//! Linking runs a `gather → resolve → link` pass around the synchronous
//! [`lashlang::LinkedModule::link`]: collect the call-paths the program
//! references but the host environment does not provide, resolve the unknowns,
//! fold `Resolved` grants into the host environment, then link. Each resolution
//! is recorded so a re-driven link reuses it without calling the resolver
//! again, and the flat Tool Catalog is never mutated — resolution is
//! link-scoped only.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    LashlangHostEnvironment, LashlangRuntimeError, lashlang_tool_contract_types,
    required_tool_lashlang_executable,
};

/// A host-authorized tool capability resolved for a deferred call-path. It
/// carries the callable contract and Lashlang identity (via the tool
/// definition) plus the host-owned Tool Execution Binding that routes a call to
/// the backing account, service, secret, or remote executor.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ToolGrant {
    /// The callable contract and Lashlang identity for the resolved tool.
    pub definition: lash_core::ToolDefinition,
    /// Optional registry source route authorized by the host. Registry-backed
    /// grants require this route at execution time; direct host providers may
    /// ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// Host-owned routing authority that connects the grant to the backing
    /// account/service/secret/executor. Opaque to the runtime; the host
    /// interprets it when fulfilling the call and when rebuilding for replay.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub execution_binding: serde_json::Value,
}

impl ToolGrant {
    pub fn new(definition: lash_core::ToolDefinition) -> Self {
        Self {
            definition,
            source_id: None,
            execution_binding: serde_json::Value::Null,
        }
    }

    pub fn with_source_id(mut self, source_id: impl Into<String>) -> Self {
        self.source_id = Some(source_id.into());
        self
    }

    pub fn with_execution_binding(mut self, execution_binding: serde_json::Value) -> Self {
        self.execution_binding = execution_binding;
        self
    }
}

/// Outcome of resolving one deferred call-path.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resolution {
    /// The call-path resolved to a host-authorized tool.
    Resolved(Box<ToolGrant>),
    /// No tool is available for the call-path; linking leaves the symbol
    /// unresolved so the model sees a clean link error.
    NotAvailable,
}

/// RLM-only, host-provided resolution of Lashlang call-paths absent from the
/// link-time host environment. The resolver resolves on demand only.
#[async_trait]
pub trait DeferredToolResolver: Send + Sync {
    /// Resolve a deterministic batch of fully-qualified Lashlang call-paths
    /// (e.g. `web.fetch`). The batch contains only paths not already provided
    /// by the host environment or recorded for this link.
    ///
    /// Resolution is non-transactional: every returned path has its own
    /// outcome, partial success is normal, and an input path omitted from the
    /// returned map is recorded as [`Resolution::NotAvailable`]. Entries for
    /// paths outside the input batch are ignored.
    async fn resolve(&self, paths: &[&str]) -> BTreeMap<String, Resolution>;

    /// Reinstall the process-local execution route for a recorded grant before
    /// it is folded into a re-driven link. This is replay rehydration only: it
    /// must not make an authorization decision or widen the recorded grant.
    ///
    /// Hosts whose grants need no process-local routing can use this default
    /// no-op implementation.
    fn install_recorded_grant(&self, _path: &str, _grant: &ToolGrant) {}
}

/// A handle to the host's deferred resolver, optional because most hosts ship
/// no deferral.
pub type SharedDeferredToolResolver = Arc<dyn DeferredToolResolver>;

/// Stable identity of one `ExecCode` link. The scope distinguishes logical
/// turns and protocol iterations, while the effect and replay keys distinguish
/// individual code effects and their durable re-drives.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeferredResolutionLinkKey {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_iteration: Option<usize>,
    pub effect_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_key: Option<String>,
}

impl DeferredResolutionLinkKey {
    pub fn from_exec_code_invocation(invocation: &lash_core::RuntimeInvocation) -> Option<Self> {
        if invocation.effect_kind() != Some(lash_core::RuntimeEffectKind::ExecCode) {
            return None;
        }
        Some(Self {
            session_id: invocation.scope.session_id.clone(),
            turn_id: invocation.scope.turn_id.clone(),
            turn_index: invocation.scope.turn_index,
            protocol_iteration: invocation.scope.protocol_iteration,
            effect_id: invocation.effect_id()?.to_string(),
            replay_key: invocation.replay_key().map(str::to_string),
        })
    }
}

/// A per-link record of every deferred resolution, keyed by call-path within
/// the execution scope. Replay/recovery applies the record so the resolver is
/// never called twice for the same link. Captures both `Resolved` grants (with
/// their Tool Execution Binding) and negative `NotAvailable` results.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct DeferredResolutionRecord {
    /// The code link whose outcomes are stored in `resolutions`. `None` is the
    /// inactive state before an executor selects its first link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_key: Option<DeferredResolutionLinkKey>,
    pub resolutions: BTreeMap<String, Resolution>,
}

impl DeferredResolutionRecord {
    /// Select the active code link, retaining outcomes only when the stable
    /// identity matches. A new link replaces the entire record so authority and
    /// negative availability results cannot leak across code effects.
    pub fn select_link(&mut self, link_key: DeferredResolutionLinkKey) {
        if self.link_key.as_ref() != Some(&link_key) {
            self.link_key = Some(link_key);
            self.resolutions.clear();
        }
    }

    /// Clear the active link when execution has no stable `ExecCode` identity.
    /// Such an invocation cannot safely reuse durable resolution outcomes.
    pub fn clear_link(&mut self) {
        self.link_key = None;
        self.resolutions.clear();
    }

    pub fn get(&self, path: &str) -> Option<&Resolution> {
        self.resolutions.get(path)
    }

    pub fn record(&mut self, path: impl Into<String>, resolution: Resolution) {
        self.resolutions.insert(path.into(), resolution);
    }

    pub fn is_empty(&self) -> bool {
        self.resolutions.is_empty()
    }
}

/// Fold a resolved [`ToolGrant`] into the host environment so the subsequent
/// link can bind its call-path. The flat catalog is untouched.
fn fold_grant(
    host_environment: &mut LashlangHostEnvironment,
    grant: &ToolGrant,
) -> Result<(), LashlangRuntimeError> {
    let binding = required_tool_lashlang_executable(&grant.definition.manifest)?;
    let operation_binding = lashlang_tool_contract_types(&grant.definition.contract);
    host_environment.resources.add_module_operation_binding(
        binding.module_path.iter().map(String::as_str),
        binding.authority_type.clone(),
        binding.operation.clone(),
        grant.definition.manifest.id.to_string(),
        operation_binding,
    );
    Ok(())
}

/// Whether the host environment already binds `call_path` (dotted
/// `module.operation`), so it does not need deferral.
fn already_provided(host_environment: &LashlangHostEnvironment, call_path: &str) -> bool {
    let Some((module_path, operation)) = call_path.rsplit_once('.') else {
        return false;
    };
    host_environment
        .resources
        .provides_module_operation(module_path, operation)
}

/// `gather → resolve`: collect every module call-path `program` references,
/// resolve the ones `host_environment` does not already provide in one
/// record-filtered batch, record the per-path outcomes, and fold `Resolved`
/// grants into `host_environment`. Returns the augmented host environment; the
/// caller links (or compiles via a cache) against it.
///
/// Every resolution is written to `record` so a re-driven or recovered link
/// replays the recorded outcomes without calling the resolver again. The flat
/// Tool Catalog is never mutated — resolution is link-scoped only.
pub async fn resolve_and_fold_deferred(
    program: &lashlang::Program,
    mut host_environment: LashlangHostEnvironment,
    resolver: Option<&SharedDeferredToolResolver>,
    record: &mut DeferredResolutionRecord,
) -> LashlangHostEnvironment {
    let referenced = lashlang::referenced_module_call_paths(program);
    let unresolved = referenced
        .into_iter()
        .filter(|path| !already_provided(&host_environment, path))
        .collect::<Vec<_>>();

    let unknown = unresolved
        .iter()
        .filter(|path| record.get(path).is_none())
        .map(String::as_str)
        .collect::<Vec<_>>();
    let mut resolved = if let Some(resolver) = resolver
        && !unknown.is_empty()
    {
        resolver.resolve(&unknown).await
    } else {
        BTreeMap::new()
    };

    for path in unresolved {
        // Replay: a recorded outcome wins and is never re-authorized. A
        // recorded grant is reinstalled into any process-local host route
        // before it is folded back into the link environment.
        let resolution = if let Some(recorded) = record.get(&path) {
            if let Resolution::Resolved(grant) = recorded
                && let Some(resolver) = resolver
            {
                resolver.install_recorded_grant(&path, grant);
            }
            recorded.clone()
        } else if resolver.is_some() {
            let resolution = resolved.remove(&path).unwrap_or(Resolution::NotAvailable);
            record.record(path.clone(), resolution.clone());
            resolution
        } else {
            // No resolver: leave the symbol unresolved for a clean link error.
            continue;
        };
        if let Resolution::Resolved(grant) = resolution {
            // A corrupt grant is treated as a clean unresolved link.
            let _ = fold_grant(&mut host_environment, &grant);
        }
    }

    host_environment
}

/// `gather → resolve → link`: [`resolve_and_fold_deferred`] then link. Used by
/// callers that do not maintain their own compile cache. `NotAvailable` (and no
/// resolver) leaves the symbol unresolved, surfacing a clean model-visible link
/// error.
pub async fn link_with_deferred_resolution(
    program: lashlang::Program,
    host_environment: LashlangHostEnvironment,
    resolver: Option<&SharedDeferredToolResolver>,
    record: &mut DeferredResolutionRecord,
) -> Result<lashlang::LinkedModule, lashlang::LinkError> {
    let host_environment =
        resolve_and_fold_deferred(&program, host_environment, resolver, record).await;
    lashlang::LinkedModule::link(program, host_environment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LashlangSurface, LashlangToolBinding, ToolDefinitionLashlangExt};
    use lash_sansio::sync::MutexExt;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn grant(name: &str, module: &str, operation: &str) -> ToolGrant {
        let definition = lash_core::ToolDefinition::raw(
            format!("tool:{name}"),
            name,
            format!("Tool {name}"),
            lash_core::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "string" }),
        )
        .with_lashlang_binding(LashlangToolBinding::new([module], operation));
        ToolGrant::new(definition).with_execution_binding(serde_json::json!({ "account": name }))
    }

    struct CountingResolver {
        grant: ToolGrant,
        calls: Arc<AtomicUsize>,
        batches: Arc<Mutex<Vec<Vec<String>>>>,
        installed: Arc<Mutex<Vec<(String, ToolGrant)>>>,
    }

    #[async_trait]
    impl DeferredToolResolver for CountingResolver {
        async fn resolve(&self, paths: &[&str]) -> BTreeMap<String, Resolution> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.batches
                .lock_recover()
                .push(paths.iter().map(|path| (*path).to_string()).collect());
            paths
                .iter()
                .filter(|path| **path == "web.fetch")
                .map(|path| {
                    (
                        (*path).to_string(),
                        Resolution::Resolved(Box::new(self.grant.clone())),
                    )
                })
                .collect()
        }

        fn install_recorded_grant(&self, path: &str, grant: &ToolGrant) {
            self.installed
                .lock_recover()
                .push((path.to_string(), grant.clone()));
        }
    }

    struct ResolverHarness {
        resolver: SharedDeferredToolResolver,
        calls: Arc<AtomicUsize>,
        batches: Arc<Mutex<Vec<Vec<String>>>>,
        installed: Arc<Mutex<Vec<(String, ToolGrant)>>>,
    }

    fn resolver_harness() -> ResolverHarness {
        let calls = Arc::new(AtomicUsize::new(0));
        let batches = Arc::new(Mutex::new(Vec::new()));
        let installed = Arc::new(Mutex::new(Vec::new()));
        let resolver = Arc::new(CountingResolver {
            grant: grant("fetch_url", "web", "fetch"),
            calls: Arc::clone(&calls),
            batches: Arc::clone(&batches),
            installed: Arc::clone(&installed),
        });
        ResolverHarness {
            resolver,
            calls,
            batches,
            installed,
        }
    }

    fn empty_host_environment() -> LashlangHostEnvironment {
        let catalog = lash_core::ToolCatalog::default();
        LashlangSurface::default()
            .host_environment(&catalog)
            .expect("empty host environment")
    }

    #[test]
    fn deferred_grant_imports_declared_schema_types() {
        let definition = lash_core::ToolDefinition::raw(
            "tool:fetch_url",
            "fetch_url",
            "Fetch a URL",
            serde_json::json!({
                "type": "object",
                "properties": { "url": { "type": "string" } },
                "required": ["url"],
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "boolean" }),
        )
        .with_lashlang_binding(
            LashlangToolBinding::new(["web"], "fetch").with_authority_type("Web"),
        );
        let grant = ToolGrant::new(definition);
        let mut environment = empty_host_environment();

        fold_grant(&mut environment, &grant).expect("grant folds");

        let operation = environment
            .resources
            .resolve_operation("Web", "fetch")
            .expect("deferred operation is registered");
        assert_eq!(
            operation.input_ty,
            lashlang::TypeExpr::Object(vec![lashlang::TypeField {
                name: "url".into(),
                ty: lashlang::TypeExpr::Str,
                optional: false,
            }])
        );
        assert_eq!(operation.output_ty, lashlang::TypeExpr::Bool);
    }

    #[tokio::test]
    async fn resolves_deferred_call_path_and_records_grant() {
        let harness = resolver_harness();
        let program = lashlang::parse(r#"await web.fetch({ url: "x" })?"#).expect("parse");
        let mut record = DeferredResolutionRecord::default();

        link_with_deferred_resolution(
            program,
            empty_host_environment(),
            Some(&harness.resolver),
            &mut record,
        )
        .await
        .expect("deferred resolution links");

        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *harness.batches.lock_recover(),
            vec![vec!["web.fetch".to_string()]]
        );
        assert!(harness.installed.lock_recover().is_empty());
        assert!(matches!(
            record.get("web.fetch"),
            Some(Resolution::Resolved(_))
        ));
    }

    #[tokio::test]
    async fn replay_reuses_record_without_calling_resolver() {
        let harness = resolver_harness();
        let program = lashlang::parse(r#"await web.fetch({ url: "x" })?"#).expect("parse");

        let mut record = DeferredResolutionRecord::default();
        link_with_deferred_resolution(
            program.clone(),
            empty_host_environment(),
            Some(&harness.resolver),
            &mut record,
        )
        .await
        .expect("first link");
        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        assert!(harness.installed.lock_recover().is_empty());

        // Re-drive the same link with the recorded resolutions: the resolver is
        // never called again.
        link_with_deferred_resolution(
            program,
            empty_host_environment(),
            Some(&harness.resolver),
            &mut record,
        )
        .await
        .expect("replayed link");
        assert_eq!(
            harness.calls.load(Ordering::SeqCst),
            1,
            "replay must not re-resolve"
        );
        let installed = harness.installed.lock_recover();
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].0, "web.fetch");
        assert_eq!(
            installed[0].1.execution_binding,
            serde_json::json!({ "account": "fetch_url" })
        );
    }

    #[tokio::test]
    async fn not_available_surfaces_clean_link_error_and_is_recorded() {
        let harness = resolver_harness();
        let program = lashlang::parse(r#"await mystery.run({})?"#).expect("parse");
        let mut record = DeferredResolutionRecord::default();

        let err = link_with_deferred_resolution(
            program.clone(),
            empty_host_environment(),
            Some(&harness.resolver),
            &mut record,
        )
        .await
        .expect_err("unavailable call-path must surface a link error");
        assert!(!format!("{err:?}").is_empty());
        assert!(matches!(
            record.get("mystery.run"),
            Some(Resolution::NotAvailable)
        ));

        // Replay reuses the recorded NotAvailable without re-resolving.
        let calls_before = harness.calls.load(Ordering::SeqCst);
        link_with_deferred_resolution(
            program,
            empty_host_environment(),
            Some(&harness.resolver),
            &mut record,
        )
        .await
        .expect_err("replayed unavailable call-path still errors");
        assert_eq!(harness.calls.load(Ordering::SeqCst), calls_before);
        assert!(harness.installed.lock_recover().is_empty());
    }

    #[tokio::test]
    async fn resolves_unknown_paths_in_one_record_filtered_batch() {
        let harness = resolver_harness();
        let program =
            lashlang::parse("await web.fetch({})?\nawait mystery.run({})?\nawait web.fetch({})?")
                .expect("parse");
        let mut record = DeferredResolutionRecord::default();

        let host = resolve_and_fold_deferred(
            &program,
            empty_host_environment(),
            Some(&harness.resolver),
            &mut record,
        )
        .await;

        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *harness.batches.lock_recover(),
            vec![vec!["mystery.run".to_string(), "web.fetch".to_string()]]
        );
        assert!(host.resources.provides_module_operation("web", "fetch"));
        assert!(!host.resources.provides_module_operation("mystery", "run"));
        assert!(matches!(
            record.get("mystery.run"),
            Some(Resolution::NotAvailable)
        ));

        // Every referenced path now has a recorded outcome, so the filtered
        // unknown bag is empty and no second batch is sent. Only the recorded
        // positive grant is replay-installed.
        let replayed = resolve_and_fold_deferred(
            &program,
            empty_host_environment(),
            Some(&harness.resolver),
            &mut record,
        )
        .await;
        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        assert!(replayed.resources.provides_module_operation("web", "fetch"));
        assert_eq!(harness.installed.lock_recover().len(), 1);
    }

    #[tokio::test]
    async fn excludes_recorded_paths_from_a_non_empty_batch() {
        let harness = resolver_harness();
        let program =
            lashlang::parse("await web.fetch({})?\nawait mystery.run({})?").expect("parse");
        let mut record = DeferredResolutionRecord::default();
        record.record(
            "web.fetch",
            Resolution::Resolved(Box::new(grant("fetch_url", "web", "fetch"))),
        );

        let host = resolve_and_fold_deferred(
            &program,
            empty_host_environment(),
            Some(&harness.resolver),
            &mut record,
        )
        .await;

        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *harness.batches.lock_recover(),
            vec![vec!["mystery.run".to_string()]]
        );
        assert_eq!(harness.installed.lock_recover().len(), 1);
        assert!(host.resources.provides_module_operation("web", "fetch"));
        assert!(matches!(
            record.get("mystery.run"),
            Some(Resolution::NotAvailable)
        ));
    }
}
