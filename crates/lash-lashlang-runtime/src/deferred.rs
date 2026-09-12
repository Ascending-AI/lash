//! RLM-only deferred tool resolution.
//!
//! A host-provided [`DeferredToolResolver`] resolves Lashlang call-paths absent
//! from the link-time [`LashlangHostEnvironment`] into [`ToolGrant`] values
//! (which carry their Tool Execution Bindings) or reports `NotAvailable`. The
//! resolver resolves on demand only — it does not enumerate, advertise, or rank
//! tools.
//!
//! Linking runs a `gather → journal → restore → link` pass around the
//! synchronous [`lashlang::LinkedModule::link`]. The durable outcome masks any
//! later ambient binding for the same call-path before a captured grant is
//! restored and folded. A re-driven link therefore reuses its journaled
//! decision without calling the resolver again, while a new admitted link may
//! observe a new ambient definition. The flat Tool Catalog is never mutated —
//! resolution is link-scoped only.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    LashlangHostEnvironment, LashlangSurface, ToolBindingError, lashlang_tool_contract_types,
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
    ///
    /// This is read-only discovery: it must not install routes, create
    /// subscriptions or processes, or perform externally visible work. A
    /// precommit retry may call it again. Route mutation belongs exclusively
    /// in [`Self::install_recorded_grant`] after the outcome is journaled.
    async fn resolve(&self, paths: &[&str]) -> BTreeMap<String, Resolution>;

    /// Install the process-local execution route for a journaled grant after
    /// its captured definition passes link-catalog validation. This is route
    /// rehydration only: it must not make an authorization decision or widen
    /// the recorded grant. Implementations must be idempotent because a later
    /// route in the same committed batch may fail and retry the whole pass.
    ///
    /// Hosts whose grants need no process-local routing can use this default
    /// no-op implementation.
    fn install_recorded_grant(
        &self,
        _path: &str,
        _grant: &ToolGrant,
    ) -> Result<(), RecordedGrantInstallError> {
        Ok(())
    }
}

/// Failure while restoring the process-local route of a journaled grant.
///
/// The distinction is part of replay behavior: a transient outage may retry
/// the same durable work identity, while revoked or incompatible routing is a
/// terminal refusal and must never be replaced by a fresh authorization.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum RecordedGrantInstallError {
    #[error("deferred tool route is temporarily unavailable: {message}")]
    Transient { message: String },
    #[error("deferred tool route was revoked: {message}")]
    Revoked { message: String },
    #[error("deferred tool route is incompatible: {message}")]
    Incompatible { message: String },
}

impl RecordedGrantInstallError {
    pub fn transient(message: impl Into<String>) -> Self {
        Self::Transient {
            message: message.into(),
        }
    }

    pub fn revoked(message: impl Into<String>) -> Self {
        Self::Revoked {
            message: message.into(),
        }
    }

    pub fn incompatible(message: impl Into<String>) -> Self {
        Self::Incompatible {
            message: message.into(),
        }
    }

    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Transient { .. })
    }
}

/// A deferred link could not restore or fold its already-decided authority.
#[derive(Debug, thiserror::Error)]
pub enum DeferredResolutionError {
    #[error("deferred resolution requires an admitted ExecCode effect address")]
    MissingLinkIdentity,
    #[error("deferred resolution record does not match the admitted ExecCode effect address")]
    LinkIdentityMismatch,
    #[error("failed to commit deferred resolution outcome: {0}")]
    Journal(#[source] lash_core::RuntimeEffectControllerError),
    #[error("journaled deferred resolution outcome is invalid: {0}")]
    InvalidJournaledOutcome(#[source] serde_json::Error),
    #[error("invalid Lashlang host tool surface: {0}")]
    Ambient(#[source] ToolBindingError),
    #[error("failed to restore recorded grant for `{path}`: {source}")]
    Install {
        path: String,
        #[source]
        source: RecordedGrantInstallError,
    },
    #[error("failed to fold recorded grant for `{path}`: {source}")]
    Fold {
        path: String,
        #[source]
        source: ToolBindingError,
    },
}

impl DeferredResolutionError {
    /// Maps replay failures onto the runtime's existing bounded retry/terminal
    /// vocabulary without discarding the route-specific typed source.
    pub fn runtime_effect_error(&self) -> lash_core::RuntimeEffectControllerError {
        let code = match self {
            Self::Journal(error) => return error.clone(),
            Self::Install {
                source: RecordedGrantInstallError::Transient { .. },
                ..
            } => lash_core::RuntimeErrorCode::RuntimeStore,
            Self::MissingLinkIdentity
            | Self::LinkIdentityMismatch
            | Self::InvalidJournaledOutcome(_)
            | Self::Ambient(_)
            | Self::Install { .. }
            | Self::Fold { .. } => lash_core::RuntimeErrorCode::ToolCatalogResolutionFailed,
        };
        lash_core::RuntimeEffectControllerError::new(code, self.to_string())
    }
}

/// Combined failure for the convenience deferred-link entry point.
#[derive(Debug, thiserror::Error)]
pub enum DeferredLinkError {
    #[error(transparent)]
    Resolution(#[from] DeferredResolutionError),
    #[error(transparent)]
    Link(#[from] lashlang::LinkError),
}

/// A handle to the host's deferred resolver, optional because most hosts ship
/// no deferral.
pub type SharedDeferredToolResolver = Arc<dyn DeferredToolResolver>;

/// Stable identity of one `ExecCode` link.
///
/// The admitted address is the whole identity: `effect_id` remains a
/// descriptive label and changing it cannot discard resolutions recorded for
/// the same durable code effect.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeferredResolutionLinkKey {
    pub address: lash_core::EffectAddress,
}

impl DeferredResolutionLinkKey {
    pub fn from_exec_code_invocation(invocation: &lash_core::RuntimeInvocation) -> Option<Self> {
        Some(Self {
            address: invocation.effect_address()?.clone(),
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
) -> Result<(), ToolBindingError> {
    let binding = required_tool_lashlang_executable(&grant.definition.manifest)?;
    let operation_binding = lashlang_tool_contract_types(&grant.definition.contract);
    host_environment.resources.add_module_operation_binding(
        binding.module_path.iter().map(String::as_str),
        binding.authority_type.clone(),
        binding.operation.clone(),
        grant.definition.manifest.id.to_string(),
        operation_binding,
    )?;
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

/// `gather → journal → restore`: collect every module call-path `program`
/// references, durably decide unresolved paths in one record-filtered batch,
/// mask paths with recorded outcomes, and fold captured `Resolved` grants into
/// `host_environment`. Returns the effective environment; the caller links (or
/// compiles via a cache) against it.
///
/// The effect journal commits before route registration or language execution;
/// `record` remains the checkpointed in-memory projection. The flat Tool
/// Catalog is never mutated — resolution is link-scoped only.
pub async fn resolve_and_fold_deferred(
    program: &lashlang::Program,
    mut host_environment: LashlangHostEnvironment,
    resolver: Option<&SharedDeferredToolResolver>,
    record: &mut DeferredResolutionRecord,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
) -> Result<LashlangHostEnvironment, DeferredResolutionError> {
    let referenced = lashlang::referenced_module_call_paths(program);
    let ambient_paths = referenced
        .iter()
        .filter(|path| already_provided(&host_environment, path))
        .cloned()
        .collect();
    let outcomes =
        journal_deferred_outcomes(referenced, move || Ok(ambient_paths), resolver, record, ctx)
            .await?;
    apply_deferred_outcomes(&mut host_environment, &outcomes, resolver, ctx)?;
    record.resolutions = outcomes;

    Ok(host_environment)
}

/// Production deferred-link path. Live resolution classifies availability from
/// the complete unmasked environment, including runtime-supplied built-ins.
/// Journal replay skips that live build, so a committed decision can still
/// mask every later claimant for its exact path before catalog collision
/// validation runs.
pub async fn resolve_and_build_deferred_environment(
    program: &lashlang::Program,
    surface: &LashlangSurface,
    catalog: &lash_core::ToolCatalog,
    resolver: Option<&SharedDeferredToolResolver>,
    record: &mut DeferredResolutionRecord,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
) -> Result<LashlangHostEnvironment, DeferredResolutionError> {
    let referenced = lashlang::referenced_module_call_paths(program);
    if referenced.is_empty() {
        return surface
            .host_environment(catalog)
            .map_err(DeferredResolutionError::Ambient);
    }
    let referenced_for_ambient = referenced.clone();
    let outcomes = journal_deferred_outcomes(
        referenced,
        move || {
            let host_environment = surface.host_environment(catalog)?;
            Ok(referenced_for_ambient
                .iter()
                .filter(|path| already_provided(&host_environment, path))
                .cloned()
                .collect())
        },
        resolver,
        record,
        ctx,
    )
    .await?;
    let masked_paths = outcomes.keys().cloned().collect::<BTreeSet<_>>();
    let mut host_environment = surface
        .host_environment_masking(catalog, &masked_paths)
        .map_err(DeferredResolutionError::Ambient)?;
    apply_deferred_outcomes(&mut host_environment, &outcomes, resolver, ctx)?;
    record.resolutions = outcomes;

    Ok(host_environment)
}

async fn journal_deferred_outcomes<F>(
    referenced: BTreeSet<String>,
    ambient_paths: F,
    resolver: Option<&SharedDeferredToolResolver>,
    record: &DeferredResolutionRecord,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
) -> Result<BTreeMap<String, Resolution>, DeferredResolutionError>
where
    F: FnOnce() -> Result<BTreeSet<String>, ToolBindingError> + Send,
{
    let link_key = record
        .link_key
        .as_ref()
        .ok_or(DeferredResolutionError::MissingLinkIdentity)?;
    let admitted_address = ctx
        .parent_invocation()
        .and_then(lash_core::RuntimeInvocation::effect_address)
        .ok_or(DeferredResolutionError::MissingLinkIdentity)?;
    if admitted_address != &link_key.address {
        return Err(DeferredResolutionError::LinkIdentityMismatch);
    }
    let effect_id = format!("{}:deferred-tool-resolution", link_key.address.replay_key);
    let operation = format!(
        "deferred_tool_resolution:v1:{}",
        serde_json::to_string(&referenced)
            .expect("deferred call-path strings encode as canonical JSON")
    );
    let recorded = record.resolutions.clone();
    let phase_context = ctx.clone();
    let resolver_for_resolution = resolver.cloned();
    let referenced_for_resolution = referenced.clone();
    let ambient_error = Arc::new(tokio::sync::Mutex::new(None));
    let ambient_error_for_resolution = Arc::clone(&ambient_error);
    let journaled = ctx
        .journaled_deferred_resolution_with(effect_id, operation, move || async move {
            let ambient_paths = match ambient_paths() {
                Ok(paths) => paths,
                Err(error) => {
                    let message = error.to_string();
                    *ambient_error_for_resolution.lock().await = Some(error);
                    return Err(lash_core::RuntimeEffectControllerError::new(
                        lash_core::RuntimeErrorCode::ToolCatalogResolutionFailed,
                        message,
                    ));
                }
            };
            let mut outcomes = BTreeMap::new();
            let mut unknown = Vec::new();
            for path in &referenced_for_resolution {
                if let Some(resolution) = recorded.get(path) {
                    outcomes.insert(path.clone(), resolution.clone());
                } else if !ambient_paths.contains(path) {
                    unknown.push(path.as_str());
                }
            }
            if let Some(resolver) = resolver_for_resolution.as_ref()
                && !unknown.is_empty()
            {
                let mut resolved = resolver.resolve(&unknown).await;
                for path in unknown {
                    outcomes.insert(
                        path.to_string(),
                        resolved.remove(path).unwrap_or(Resolution::NotAvailable),
                    );
                }
            }
            let _phase =
                phase_context.named_phase("rlm_lashlang.deferred_resolve.after_resolver_return");
            serde_json::to_value(outcomes).map_err(|error| {
                lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::RecordEncodingFailed,
                    format!("failed to encode deferred resolution outcome: {error}"),
                )
            })
        })
        .await;
    if let Some(error) = ambient_error.lock().await.take() {
        return Err(DeferredResolutionError::Ambient(error));
    }
    let journaled = journaled.map_err(DeferredResolutionError::Journal)?;
    {
        let _phase = ctx.named_phase("rlm_lashlang.deferred_resolve.after_durable_record");
    }
    serde_json::from_value(journaled).map_err(DeferredResolutionError::InvalidJournaledOutcome)
}

fn apply_deferred_outcomes(
    host_environment: &mut LashlangHostEnvironment,
    outcomes: &BTreeMap<String, Resolution>,
    resolver: Option<&SharedDeferredToolResolver>,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
) -> Result<(), DeferredResolutionError> {
    // Recorded authority is applied before ambient availability is consulted:
    // exact paths are masked first, negative outcomes remain absent, and
    // positive outcomes fold their captured definitions through the catalog's
    // ordinary collision checks before any process-local route is installed.
    for path in outcomes.keys() {
        if let Some((module_path, operation)) = path.rsplit_once('.') {
            host_environment
                .resources
                .mask_module_operation(module_path, operation);
        }
    }
    for (path, resolution) in outcomes {
        let Resolution::Resolved(grant) = resolution else {
            continue;
        };
        fold_grant(host_environment, grant).map_err(|source| DeferredResolutionError::Fold {
            path: path.clone(),
            source,
        })?;
    }
    for (path, resolution) in outcomes {
        let Resolution::Resolved(grant) = resolution else {
            continue;
        };
        let Some(resolver) = resolver else {
            continue;
        };
        {
            let _phase = ctx.named_phase("rlm_lashlang.deferred_resolve.before_registration");
            resolver
                .install_recorded_grant(path, grant)
                .map_err(|source| DeferredResolutionError::Install {
                    path: path.clone(),
                    source,
                })?;
        }
    }
    Ok(())
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
    ctx: &lash_core::RuntimeExecutionContext<'_>,
) -> Result<lashlang::LinkedModule, DeferredLinkError> {
    let host_environment =
        resolve_and_fold_deferred(&program, host_environment, resolver, record, ctx).await?;
    Ok(lashlang::LinkedModule::link(program, host_environment)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LashlangSurface, ToolBinding, ToolDefinitionBindingExt};
    use lash_sansio::sync::MutexExt;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    mod runtime_built_in;

    #[derive(Clone, Copy)]
    enum JournalFault {
        None,
        AfterResolverReturn,
        AfterDurableRecord,
    }

    struct FaultJournalController {
        fault: JournalFault,
        faults_remaining: AtomicUsize,
        outcomes: Mutex<BTreeMap<String, lash_core::RuntimeEffectOutcome>>,
    }

    impl FaultJournalController {
        fn new(fault: JournalFault) -> Self {
            Self {
                fault,
                faults_remaining: AtomicUsize::new(usize::from(!matches!(
                    fault,
                    JournalFault::None
                ))),
                outcomes: Mutex::new(BTreeMap::new()),
            }
        }
    }

    impl lash_core::AwaitEventResolver for FaultJournalController {}

    #[async_trait]
    impl lash_core::RuntimeEffectController for FaultJournalController {
        async fn execute_effect(
            &self,
            envelope: lash_core::RuntimeEffectEnvelope,
            local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
        ) -> Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError>
        {
            let lash_core::RuntimeEffectCommand::LanguageRuntimeValue { operation } =
                &envelope.command
            else {
                return local_executor.execute(envelope).await;
            };
            if !operation.starts_with("deferred_tool_resolution:v1:") {
                return local_executor.execute(envelope).await;
            }
            let key = envelope.invocation.replay_key().to_string();
            if let Some(outcome) = self.outcomes.lock_recover().get(&key).cloned() {
                return Ok(outcome);
            }
            let outcome = local_executor.execute(envelope).await?;
            let inject = self
                .faults_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok();
            if inject && matches!(self.fault, JournalFault::AfterResolverReturn) {
                return Err(lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::RuntimeStore,
                    "injected failure after resolver return",
                ));
            }
            self.outcomes.lock_recover().insert(key, outcome.clone());
            if inject && matches!(self.fault, JournalFault::AfterDurableRecord) {
                return Err(lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::RuntimeStore,
                    "injected failure after durable record",
                ));
            }
            Ok(outcome)
        }
    }

    #[test]
    fn deferred_link_identity_ignores_descriptive_label_and_attribution() {
        let address = lash_core::EffectAddress::new(
            lash_core::ExecutionScope::turn("session", "turn"),
            "exec-code:0",
        )
        .expect("valid deferred-link address");
        let invocation = |effect_id: &str, turn_index, protocol_iteration| {
            lash_core::RuntimeInvocation::effect(
                address.clone(),
                lash_core::RuntimeAttribution::for_turn(
                    "session",
                    "turn",
                    turn_index,
                    protocol_iteration,
                ),
                effect_id,
            )
        };

        assert_eq!(
            DeferredResolutionLinkKey::from_exec_code_invocation(&invocation("first", 0, 0)),
            DeferredResolutionLinkKey::from_exec_code_invocation(&invocation("renamed", 7, 11)),
        );
        assert_eq!(
            serde_json::to_value(
                DeferredResolutionLinkKey::from_exec_code_invocation(&invocation("first", 0, 0))
                    .expect("effect invocation has a link identity")
            )
            .expect("link identity encodes"),
            serde_json::json!({"address": address})
        );
    }

    fn grant(name: &str, module: &str, operation: &str) -> ToolGrant {
        let definition = lash_core::ToolDefinition::raw(
            format!("tool:{name}"),
            name,
            format!("Tool {name}"),
            lash_core::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "string" }),
        )
        .with_tool_binding(ToolBinding::new([module], operation));
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

        fn install_recorded_grant(
            &self,
            path: &str,
            grant: &ToolGrant,
        ) -> Result<(), RecordedGrantInstallError> {
            self.installed
                .lock_recover()
                .push((path.to_string(), grant.clone()));
            Ok(())
        }
    }

    struct ResolverHarness {
        resolver: SharedDeferredToolResolver,
        calls: Arc<AtomicUsize>,
        batches: Arc<Mutex<Vec<Vec<String>>>>,
        installed: Arc<Mutex<Vec<(String, ToolGrant)>>>,
    }

    struct TransientInstallResolver {
        calls: AtomicUsize,
        installs: AtomicUsize,
        captured: ToolGrant,
    }

    struct RevokedInstallResolver {
        calls: AtomicUsize,
        current: Mutex<ToolGrant>,
        installed_ids: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl DeferredToolResolver for RevokedInstallResolver {
        async fn resolve(&self, paths: &[&str]) -> BTreeMap<String, Resolution> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let current = self.current.lock_recover().clone();
            paths
                .iter()
                .map(|path| {
                    (
                        (*path).to_string(),
                        Resolution::Resolved(Box::new(current.clone())),
                    )
                })
                .collect()
        }

        fn install_recorded_grant(
            &self,
            _path: &str,
            grant: &ToolGrant,
        ) -> Result<(), RecordedGrantInstallError> {
            self.installed_ids
                .lock_recover()
                .push(grant.definition.id().to_string());
            Err(RecordedGrantInstallError::revoked(
                "injected revoked account",
            ))
        }
    }

    #[async_trait]
    impl DeferredToolResolver for TransientInstallResolver {
        async fn resolve(&self, paths: &[&str]) -> BTreeMap<String, Resolution> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            paths
                .iter()
                .map(|path| {
                    (
                        (*path).to_string(),
                        Resolution::Resolved(Box::new(self.captured.clone())),
                    )
                })
                .collect()
        }

        fn install_recorded_grant(
            &self,
            _path: &str,
            grant: &ToolGrant,
        ) -> Result<(), RecordedGrantInstallError> {
            assert_eq!(grant.definition.id(), self.captured.definition.id());
            if self.installs.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(RecordedGrantInstallError::transient(
                    "injected failure before registration",
                ))
            } else {
                Ok(())
            }
        }
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

    fn surface_with_shared_fetch_modules(modules: &[&str]) -> LashlangSurface {
        let mut resources = lashlang::LashlangHostCatalog::new();
        for module in modules {
            resources
                .add_module_operation(
                    [*module],
                    "SharedFetch",
                    "fetch",
                    format!("surface:{module}.fetch"),
                    lashlang::TypeExpr::Any,
                    lashlang::TypeExpr::Bool,
                )
                .expect("surface test binding is valid");
        }
        LashlangSurface {
            resources,
            ..LashlangSurface::default()
        }
    }

    fn incompatible_shared_fetch_catalog() -> lash_core::ToolCatalog {
        lash_core::ToolCatalog::from_tool_definitions(vec![
            lash_core::ToolDefinition::raw(
                "tool:catalog_fetch",
                "catalog_fetch",
                "Catalog fetch",
                lash_core::ToolDefinition::default_input_schema(),
                serde_json::json!({ "type": "string" }),
            )
            .with_tool_binding(
                ToolBinding::new(["catalog"], "fetch").with_authority_type("SharedFetch"),
            ),
        ])
    }

    fn link_context(
        record: &mut DeferredResolutionRecord,
    ) -> lash_core::RuntimeExecutionContext<'static> {
        link_context_with_controller(
            record,
            "exec-code:0",
            Arc::new(FaultJournalController::new(JournalFault::None)),
        )
    }

    fn link_context_with_controller(
        record: &mut DeferredResolutionRecord,
        replay_key: &str,
        controller: Arc<dyn lash_core::RuntimeEffectController>,
    ) -> lash_core::RuntimeExecutionContext<'static> {
        let invocation = lash_core::testing::exec_code_invocation(
            "session",
            "turn",
            0,
            0,
            "exec-code",
            replay_key,
        );
        let link_key = DeferredResolutionLinkKey::from_exec_code_invocation(&invocation)
            .expect("effect invocation has a link identity");
        if record.link_key.is_none() {
            record.link_key = Some(link_key);
        } else {
            record.select_link(link_key);
        }
        lash_core::testing::code_execution_context_with_effect_controller_and_invocation(
            controller, invocation,
        )
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
        .with_tool_binding(ToolBinding::new(["web"], "fetch").with_authority_type("Web"));
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
        let ctx = link_context(&mut record);

        link_with_deferred_resolution(
            program,
            empty_host_environment(),
            Some(&harness.resolver),
            &mut record,
            &ctx,
        )
        .await
        .expect("deferred resolution links");

        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *harness.batches.lock_recover(),
            vec![vec!["web.fetch".to_string()]]
        );
        assert_eq!(harness.installed.lock_recover().len(), 1);
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
        let ctx = link_context(&mut record);
        link_with_deferred_resolution(
            program.clone(),
            empty_host_environment(),
            Some(&harness.resolver),
            &mut record,
            &ctx,
        )
        .await
        .expect("first link");
        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        assert_eq!(harness.installed.lock_recover().len(), 1);

        // Re-drive the same link with the recorded resolutions: the resolver is
        // never called again.
        link_with_deferred_resolution(
            program,
            empty_host_environment(),
            Some(&harness.resolver),
            &mut record,
            &ctx,
        )
        .await
        .expect("replayed link");
        assert_eq!(
            harness.calls.load(Ordering::SeqCst),
            1,
            "replay must not re-resolve"
        );
        let installed = harness.installed.lock_recover();
        assert_eq!(installed.len(), 2);
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
        let ctx = link_context(&mut record);

        let err = link_with_deferred_resolution(
            program.clone(),
            empty_host_environment(),
            Some(&harness.resolver),
            &mut record,
            &ctx,
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
            &ctx,
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
        let ctx = link_context(&mut record);

        let host = resolve_and_fold_deferred(
            &program,
            empty_host_environment(),
            Some(&harness.resolver),
            &mut record,
            &ctx,
        )
        .await
        .expect("resolution succeeds");

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
            &ctx,
        )
        .await
        .expect("replay succeeds");
        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        assert!(replayed.resources.provides_module_operation("web", "fetch"));
        assert_eq!(harness.installed.lock_recover().len(), 2);
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
        let ctx = link_context(&mut record);

        let host = resolve_and_fold_deferred(
            &program,
            empty_host_environment(),
            Some(&harness.resolver),
            &mut record,
            &ctx,
        )
        .await
        .expect("resolution succeeds");

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

    #[tokio::test]
    async fn recorded_unavailable_masks_a_new_ambient_binding() {
        let program = lashlang::parse(r#"await web.fetch({ url: "x" })?"#).expect("parse");
        let mut ambient = empty_host_environment();
        fold_grant(&mut ambient, &grant("ambient_fetch", "web", "fetch"))
            .expect("ambient grant folds");
        let mut record = DeferredResolutionRecord::default();
        record.record("web.fetch", Resolution::NotAvailable);
        let ctx = link_context(&mut record);

        link_with_deferred_resolution(program, ambient, None, &mut record, &ctx)
            .await
            .expect_err("the recorded negative outcome must mask the ambient replacement");
    }

    #[tokio::test]
    async fn recorded_grant_replaces_a_changed_ambient_binding() {
        let harness = resolver_harness();
        let program = lashlang::parse(r#"await web.fetch({ url: "x" })?"#).expect("parse");
        let mut ambient = empty_host_environment();
        fold_grant(&mut ambient, &grant("ambient_fetch", "web", "fetch"))
            .expect("ambient grant folds");
        let captured = ToolGrant::new(
            lash_core::ToolDefinition::raw(
                "tool:captured_fetch",
                "captured_fetch",
                "Captured fetch",
                lash_core::ToolDefinition::default_input_schema(),
                serde_json::json!({ "type": "boolean" }),
            )
            .with_tool_binding(ToolBinding::new(["web"], "fetch")),
        );
        let captured_id = captured.definition.manifest.id.to_string();
        let mut record = DeferredResolutionRecord::default();
        record.record("web.fetch", Resolution::Resolved(Box::new(captured)));
        let ctx = link_context(&mut record);

        let effective = resolve_and_fold_deferred(
            &program,
            ambient,
            Some(&harness.resolver),
            &mut record,
            &ctx,
        )
        .await
        .expect("resolution succeeds");
        let module = effective
            .resources
            .resolve_module_path(&["web"])
            .expect("web module remains available");
        let binding = effective
            .resources
            .resolve_module_operation(&module.resource_type, "web", "fetch")
            .expect("recorded web.fetch binding is installed");

        assert_eq!(binding.host_operation, captured_id);
        assert_eq!(binding.binding.output_ty, lashlang::TypeExpr::Bool);
        assert_eq!(harness.calls.load(Ordering::SeqCst), 0);
        assert_eq!(harness.installed.lock_recover().len(), 1);
    }

    #[tokio::test]
    async fn recorded_grant_still_obeys_unrelated_ambient_module_collisions() {
        let harness = resolver_harness();
        let program = lashlang::parse(r#"await web.fetch({})?"#).expect("parse");
        let mut ambient = empty_host_environment();
        for operation in ["fetch", "post"] {
            let definition = lash_core::ToolDefinition::raw(
                format!("tool:ambient_{operation}"),
                format!("ambient_{operation}"),
                "Ambient operation",
                lash_core::ToolDefinition::default_input_schema(),
                serde_json::json!({ "type": "string" }),
            )
            .with_tool_binding(
                ToolBinding::new(["web"], operation).with_authority_type("AmbientWeb"),
            );
            fold_grant(&mut ambient, &ToolGrant::new(definition)).expect("ambient operation folds");
        }
        let captured = ToolGrant::new(
            lash_core::ToolDefinition::raw(
                "tool:captured_fetch",
                "captured_fetch",
                "Captured fetch",
                lash_core::ToolDefinition::default_input_schema(),
                serde_json::json!({ "type": "string" }),
            )
            .with_tool_binding(
                ToolBinding::new(["web"], "fetch").with_authority_type("CapturedWeb"),
            ),
        );
        let mut record = DeferredResolutionRecord::default();
        record.record("web.fetch", Resolution::Resolved(Box::new(captured)));
        let ctx = link_context(&mut record);

        let error = resolve_and_fold_deferred(
            &program,
            ambient,
            Some(&harness.resolver),
            &mut record,
            &ctx,
        )
        .await
        .expect_err("the unrelated ambient web.post binding keeps its module authority");
        assert!(matches!(error, DeferredResolutionError::Fold { .. }));
        assert_eq!(harness.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn fault_after_resolver_return_allows_side_effect_free_rediscovery() {
        let harness = resolver_harness();
        let controller = Arc::new(FaultJournalController::new(
            JournalFault::AfterResolverReturn,
        ));
        let program = lashlang::parse(r#"await web.fetch({ url: "x" })?"#).expect("parse");
        let mut record = DeferredResolutionRecord::default();
        let ctx =
            link_context_with_controller(&mut record, "exec-code:fault-before", controller.clone());

        let first = resolve_and_fold_deferred(
            &program,
            empty_host_environment(),
            Some(&harness.resolver),
            &mut record,
            &ctx,
        )
        .await;
        assert!(matches!(first, Err(DeferredResolutionError::Journal(_))));
        assert!(record.resolutions.is_empty());
        assert!(harness.installed.lock_recover().is_empty());

        let mut restarted_record = DeferredResolutionRecord::default();
        let restarted_ctx = link_context_with_controller(
            &mut restarted_record,
            "exec-code:fault-before",
            controller,
        );
        resolve_and_fold_deferred(
            &program,
            empty_host_environment(),
            Some(&harness.resolver),
            &mut restarted_record,
            &restarted_ctx,
        )
        .await
        .expect("precommit retry resolves again and then commits");
        assert_eq!(harness.calls.load(Ordering::SeqCst), 2);
        assert_eq!(harness.installed.lock_recover().len(), 1);
    }

    #[tokio::test]
    async fn fault_after_durable_record_replays_without_resolving() {
        let harness = resolver_harness();
        let controller = Arc::new(FaultJournalController::new(
            JournalFault::AfterDurableRecord,
        ));
        let program = lashlang::parse(r#"await web.fetch({ url: "x" })?"#).expect("parse");
        let mut record = DeferredResolutionRecord::default();
        let ctx =
            link_context_with_controller(&mut record, "exec-code:fault-after", controller.clone());

        let first = resolve_and_fold_deferred(
            &program,
            empty_host_environment(),
            Some(&harness.resolver),
            &mut record,
            &ctx,
        )
        .await;
        assert!(matches!(first, Err(DeferredResolutionError::Journal(_))));
        assert!(record.resolutions.is_empty());

        let mut restarted_record = DeferredResolutionRecord::default();
        let restarted_ctx = link_context_with_controller(
            &mut restarted_record,
            "exec-code:fault-after",
            controller,
        );
        resolve_and_fold_deferred(
            &program,
            empty_host_environment(),
            Some(&harness.resolver),
            &mut restarted_record,
            &restarted_ctx,
        )
        .await
        .expect("postcommit retry replays the durable outcome");
        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        assert_eq!(harness.installed.lock_recover().len(), 1);
    }

    #[tokio::test]
    async fn journal_replay_masks_changed_ambient_before_live_lookup() {
        let harness = resolver_harness();
        let controller = Arc::new(FaultJournalController::new(JournalFault::None));
        let program = lashlang::parse(r#"await web.fetch({ url: "x" })?"#).expect("parse");
        let mut first_record = DeferredResolutionRecord::default();
        let first_ctx = link_context_with_controller(
            &mut first_record,
            "exec-code:journal-replay",
            controller.clone(),
        );
        resolve_and_fold_deferred(
            &program,
            empty_host_environment(),
            Some(&harness.resolver),
            &mut first_record,
            &first_ctx,
        )
        .await
        .expect("first resolution commits");

        let mut changed_ambient = empty_host_environment();
        fold_grant(
            &mut changed_ambient,
            &grant("ambient_replacement", "web", "fetch"),
        )
        .expect("replacement folds into ambient environment");
        let mut replayed_record = DeferredResolutionRecord::default();
        let replay_ctx = link_context_with_controller(
            &mut replayed_record,
            "exec-code:journal-replay",
            controller,
        );
        let effective = resolve_and_fold_deferred(
            &program,
            changed_ambient,
            Some(&harness.resolver),
            &mut replayed_record,
            &replay_ctx,
        )
        .await
        .expect("journal outcome restores before ambient lookup");
        let module = effective
            .resources
            .resolve_module_path(&["web"])
            .expect("captured module restored");
        let binding = effective
            .resources
            .resolve_module_operation(&module.resource_type, "web", "fetch")
            .expect("captured operation restored");

        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        assert_eq!(binding.host_operation, "tool:fetch_url");
    }

    #[tokio::test]
    async fn journal_replay_masks_changed_surface_before_catalog_merge() {
        let harness = resolver_harness();
        let controller = Arc::new(FaultJournalController::new(JournalFault::None));
        let program = lashlang::parse(r#"await web.fetch({ url: "x" })?"#).expect("parse");
        let mut first_record = DeferredResolutionRecord::default();
        let first_ctx = link_context_with_controller(
            &mut first_record,
            "exec-code:surface-journal-replay",
            controller.clone(),
        );
        resolve_and_build_deferred_environment(
            &program,
            &LashlangSurface::default(),
            &lash_core::ToolCatalog::default(),
            Some(&harness.resolver),
            &mut first_record,
            &first_ctx,
        )
        .await
        .expect("first resolution commits");

        let surface = surface_with_shared_fetch_modules(&["web"]);
        let catalog = incompatible_shared_fetch_catalog();
        let mut replayed_record = DeferredResolutionRecord::default();
        let replay_ctx = link_context_with_controller(
            &mut replayed_record,
            "exec-code:surface-journal-replay",
            controller,
        );
        let effective = resolve_and_build_deferred_environment(
            &program,
            &surface,
            &catalog,
            Some(&harness.resolver),
            &mut replayed_record,
            &replay_ctx,
        )
        .await
        .expect("journaled surface path is masked before catalog merge validation");

        let web = effective
            .resources
            .resolve_module_path(&["web"])
            .expect("captured web module restored");
        let web_fetch = effective
            .resources
            .resolve_module_operation(&web.resource_type, "web", "fetch")
            .expect("captured web.fetch restored");
        let catalog_module = effective
            .resources
            .resolve_module_path(&["catalog"])
            .expect("unrelated catalog module remains");
        let catalog_fetch = effective
            .resources
            .resolve_module_operation(&catalog_module.resource_type, "catalog", "fetch")
            .expect("unrelated catalog operation remains");

        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
        assert_eq!(web_fetch.host_operation, "tool:fetch_url");
        assert_eq!(web_fetch.binding.output_ty, lashlang::TypeExpr::Str);
        assert_eq!(catalog_fetch.host_operation, "tool:catalog_fetch");
    }

    #[tokio::test]
    async fn journal_replay_preserves_unrelated_surface_catalog_collision() {
        let harness = resolver_harness();
        let controller = Arc::new(FaultJournalController::new(JournalFault::None));
        let program = lashlang::parse(r#"await web.fetch({ url: "x" })?"#).expect("parse");
        let mut first_record = DeferredResolutionRecord::default();
        let first_ctx = link_context_with_controller(
            &mut first_record,
            "exec-code:surface-unrelated-collision",
            controller.clone(),
        );
        resolve_and_build_deferred_environment(
            &program,
            &LashlangSurface::default(),
            &lash_core::ToolCatalog::default(),
            Some(&harness.resolver),
            &mut first_record,
            &first_ctx,
        )
        .await
        .expect("first resolution commits");

        let surface = surface_with_shared_fetch_modules(&["web", "unrelated"]);
        let catalog = incompatible_shared_fetch_catalog();
        let mut replayed_record = DeferredResolutionRecord::default();
        let replay_ctx = link_context_with_controller(
            &mut replayed_record,
            "exec-code:surface-unrelated-collision",
            controller,
        );
        let error = resolve_and_build_deferred_environment(
            &program,
            &surface,
            &catalog,
            Some(&harness.resolver),
            &mut replayed_record,
            &replay_ctx,
        )
        .await
        .expect_err("masking web.fetch must not hide the unrelated surface collision");

        assert!(matches!(
            error,
            DeferredResolutionError::Ambient(ToolBindingError::ConflictingBinding {
                source:
                    lashlang::LashlangHostCatalogError::ConflictingResourceOperation {
                        resource_type,
                        operation,
                    },
            }) if resource_type == "SharedFetch" && operation == "fetch"
        ));
        assert_eq!(harness.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fault_before_registration_retries_captured_route_without_resolving() {
        let resolver = Arc::new(TransientInstallResolver {
            calls: AtomicUsize::new(0),
            installs: AtomicUsize::new(0),
            captured: grant("captured_fetch", "web", "fetch"),
        });
        let shared: SharedDeferredToolResolver = resolver.clone();
        let controller = Arc::new(FaultJournalController::new(JournalFault::None));
        let program = lashlang::parse(r#"await web.fetch({ url: "x" })?"#).expect("parse");
        let mut record = DeferredResolutionRecord::default();
        let ctx = link_context_with_controller(&mut record, "exec-code:route", controller.clone());

        let first = resolve_and_fold_deferred(
            &program,
            empty_host_environment(),
            Some(&shared),
            &mut record,
            &ctx,
        )
        .await
        .expect_err("the first process-local route install is interrupted");
        assert!(matches!(
            first,
            DeferredResolutionError::Install {
                source: RecordedGrantInstallError::Transient { .. },
                ..
            }
        ));
        assert!(record.resolutions.is_empty());

        let mut restarted_record = DeferredResolutionRecord::default();
        let restarted_ctx =
            link_context_with_controller(&mut restarted_record, "exec-code:route", controller);
        let effective = resolve_and_fold_deferred(
            &program,
            empty_host_environment(),
            Some(&shared),
            &mut restarted_record,
            &restarted_ctx,
        )
        .await
        .expect("retry reinstalls the journaled route");
        assert!(
            effective
                .resources
                .provides_module_operation("web", "fetch")
        );
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolver.installs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn revoked_route_refuses_without_replacing_the_journaled_grant() {
        let resolver = Arc::new(RevokedInstallResolver {
            calls: AtomicUsize::new(0),
            current: Mutex::new(grant("captured_fetch", "web", "fetch")),
            installed_ids: Mutex::new(Vec::new()),
        });
        let shared: SharedDeferredToolResolver = resolver.clone();
        let controller = Arc::new(FaultJournalController::new(JournalFault::None));
        let program = lashlang::parse(r#"await web.fetch({ url: "x" })?"#).expect("parse");
        let mut record = DeferredResolutionRecord::default();
        let ctx = link_context_with_controller(&mut record, "exec-code:revoked", controller);

        for replacement in ["replacement_fetch", "another_fetch"] {
            let error = resolve_and_fold_deferred(
                &program,
                empty_host_environment(),
                Some(&shared),
                &mut record,
                &ctx,
            )
            .await
            .expect_err("revoked route stays terminal");
            assert!(matches!(
                error,
                DeferredResolutionError::Install {
                    source: RecordedGrantInstallError::Revoked { .. },
                    ..
                }
            ));
            *resolver.current.lock_recover() = grant(replacement, "web", "fetch");
        }

        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *resolver.installed_ids.lock_recover(),
            vec!["tool:captured_fetch", "tool:captured_fetch"]
        );
    }

    #[tokio::test]
    async fn independent_link_can_accept_a_new_ambient_binding() {
        let harness = resolver_harness();
        let program = lashlang::parse(r#"await mystery.run({})?"#).expect("parse");
        let mut first_record = DeferredResolutionRecord::default();
        let first_ctx = link_context_with_controller(
            &mut first_record,
            "exec-code:first",
            Arc::new(FaultJournalController::new(JournalFault::None)),
        );
        link_with_deferred_resolution(
            program.clone(),
            empty_host_environment(),
            Some(&harness.resolver),
            &mut first_record,
            &first_ctx,
        )
        .await
        .expect_err("first link records the unavailable decision");

        let mut ambient = empty_host_environment();
        fold_grant(&mut ambient, &grant("ambient_run", "mystery", "run"))
            .expect("new ambient definition folds");
        let mut second_record = DeferredResolutionRecord::default();
        let second_ctx = link_context_with_controller(
            &mut second_record,
            "exec-code:second",
            Arc::new(FaultJournalController::new(JournalFault::None)),
        );
        link_with_deferred_resolution(program, ambient, None, &mut second_record, &second_ctx)
            .await
            .expect("an independent link may use the new ambient definition");
        assert!(second_record.get("mystery.run").is_none());
    }

    #[tokio::test]
    async fn deferred_record_refuses_a_different_admitted_link_address() {
        let program = lashlang::parse(r#"await web.fetch({})?"#).expect("parse");
        let mut record = DeferredResolutionRecord::default();
        let _record_context = link_context_with_controller(
            &mut record,
            "exec-code:record",
            Arc::new(FaultJournalController::new(JournalFault::None)),
        );
        let mismatched_invocation = lash_core::testing::exec_code_invocation(
            "session",
            "turn",
            0,
            0,
            "exec-code",
            "exec-code:other",
        );
        let mismatched_context =
            lash_core::testing::code_execution_context_with_invocation(mismatched_invocation);

        let error = resolve_and_fold_deferred(
            &program,
            empty_host_environment(),
            None,
            &mut record,
            &mismatched_context,
        )
        .await
        .expect_err("a record cannot be replayed under another admitted address");
        assert!(matches!(
            error,
            DeferredResolutionError::LinkIdentityMismatch
        ));
    }

    #[test]
    fn route_restore_failures_map_to_bounded_retry_policy() {
        let transient = DeferredResolutionError::Install {
            path: "web.fetch".into(),
            source: RecordedGrantInstallError::transient("backend restarting"),
        }
        .runtime_effect_error();
        let revoked = DeferredResolutionError::Install {
            path: "web.fetch".into(),
            source: RecordedGrantInstallError::revoked("account disconnected"),
        }
        .runtime_effect_error();
        let incompatible = DeferredResolutionError::Install {
            path: "web.fetch".into(),
            source: RecordedGrantInstallError::incompatible("route format changed"),
        }
        .runtime_effect_error();

        assert_eq!(transient.code, lash_core::RuntimeErrorCode::RuntimeStore);
        assert!(transient.code.is_retryable());
        assert_eq!(
            revoked.code,
            lash_core::RuntimeErrorCode::ToolCatalogResolutionFailed
        );
        assert_eq!(
            incompatible.code,
            lash_core::RuntimeErrorCode::ToolCatalogResolutionFailed
        );
        assert!(revoked.code.is_terminal());
        assert!(incompatible.code.is_terminal());
    }
}
