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
//! restored and folded. A redriven link therefore reuses its journaled
//! decision without calling the resolver again, while a new admitted link may
//! observe a new ambient definition. The flat Tool Catalog is never mutated —
//! resolution is link-scoped only.

use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use lash_sansio::sync::MutexExt;

use crate::{
    LashlangHostEnvironment, LashlangSurface, ToolBindingError, lashlang_tool_operation_contract,
    required_tool_executable,
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

/// Who one deferred resolution resolves for: the execution that links, the
/// logical Run it belongs to, and the capability refs that Run's recorded
/// shape names. A deployment-wide resolver grants by this, never by a
/// resolver built for one turn.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct DeferredResolveContext<'a> {
    /// The session frame or process whose cell links.
    pub owner: &'a lash_core::ExecutionOwner,
    /// The admitted logical Run whose cell links; `None` for a process cell.
    pub run: Option<&'a lash_core_worker::TurnAddress>,
    /// The capability refs the Run's recorded shape names, by slot
    /// ([`RunSpec::capabilities`](lash_core::RunSpec::capabilities)): the
    /// same refs on every replay and cold reopen of the Run. Empty for a
    /// process cell or a Run whose spec names none.
    pub capabilities: &'a BTreeMap<lash_core::SlotId, lash_core::CapabilityRef>,
}

impl<'a> DeferredResolveContext<'a> {
    pub fn new(
        owner: &'a lash_core::ExecutionOwner,
        run: Option<&'a lash_core_worker::TurnAddress>,
        capabilities: &'a BTreeMap<lash_core::SlotId, lash_core::CapabilityRef>,
    ) -> Self {
        Self {
            owner,
            run,
            capabilities,
        }
    }
}

/// RLM-only, host-provided resolution of Lashlang call-paths absent from the
/// link-time host environment. The resolver resolves on demand only.
#[async_trait]
pub trait DeferredToolResolver: Send + Sync {
    /// Resolve a deterministic batch of fully-qualified Lashlang call-paths
    /// (e.g. `web.fetch`) for the execution `cx` names. The batch contains
    /// only paths not already provided by the host environment or recorded
    /// for this link.
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
    async fn resolve(
        &self,
        cx: &DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> BTreeMap<String, Resolution>;

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
    Ambient(#[source] Box<ToolBindingError>),
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
        source: Box<ToolBindingError>,
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
    Link(#[from] lashlang::ModuleCompileError),
    #[error("worker compilation failed: {0}")]
    Worker(String),
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

/// The in-memory projection of one journaled deferred resolution effect.
/// An active link always has its admitted identity. The journal owns its
/// outcomes; this projection is never serialized into execution state.
#[derive(Clone, Debug)]
pub struct DeferredLink {
    pub key: DeferredResolutionLinkKey,
    pub outcomes: BTreeMap<String, Resolution>,
}

impl DeferredLink {
    pub fn new(key: DeferredResolutionLinkKey) -> Self {
        Self {
            key,
            outcomes: BTreeMap::new(),
        }
    }

    pub fn select_link(&mut self, key: DeferredResolutionLinkKey) {
        if self.key != key {
            *self = Self::new(key);
        }
    }

    pub fn get(&self, path: &str) -> Option<&Resolution> {
        self.outcomes.get(path)
    }

    pub fn record(&mut self, path: impl Into<String>, resolution: Resolution) {
        self.outcomes.insert(path.into(), resolution);
    }

    pub fn is_empty(&self) -> bool {
        self.outcomes.is_empty()
    }
}

/// Fold a resolved [`ToolGrant`] into the host environment so the subsequent
/// link can bind its call-path. The flat catalog is untouched.
fn fold_grant(
    host_environment: &mut LashlangHostEnvironment,
    grant: &ToolGrant,
) -> Result<(), ToolBindingError> {
    let binding = required_tool_executable(&grant.definition.manifest)?;
    let contract = lashlang_tool_operation_contract(&grant.definition.contract);
    host_environment.resources.add_module_operation_contract(
        binding.module_path.iter().map(String::as_str),
        binding.authority_type.clone(),
        binding.operation.clone(),
        grant.definition.manifest.id.to_string(),
        &contract,
    )?;
    Ok(())
}

/// Whether the host environment already binds `call_path` (dotted
/// `module.operation`), so it does not need deferral.
fn already_provided(host_environment: &LashlangHostEnvironment, call_path: &str) -> bool {
    if host_environment
        .resources
        .provides_value_constructor(call_path)
    {
        return true;
    }
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
/// `record` remains an in-memory projection of that effect. The flat Tool
/// Catalog is never mutated — resolution is link-scoped only.
pub async fn resolve_and_fold_deferred(
    program: &lashlang::Program,
    mut host_environment: LashlangHostEnvironment,
    resolver: Option<&SharedDeferredToolResolver>,
    record: &mut DeferredLink,
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
    record.outcomes = outcomes;

    Ok(host_environment)
}

/// Production deferred-link path. Live resolution first masks outcomes retained
/// for this link, then classifies availability from the remaining environment,
/// including runtime-supplied built-ins. Journal replay skips that live build,
/// so a committed decision likewise masks every later claimant for its exact
/// path before catalog collision validation runs.
pub async fn resolve_and_build_deferred_environment(
    program: &lashlang::Program,
    surface: &LashlangSurface,
    catalog: &lash_core::ToolCatalog,
    resolver: Option<&SharedDeferredToolResolver>,
    record: &mut DeferredLink,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
) -> Result<LashlangHostEnvironment, DeferredResolutionError> {
    let referenced = lashlang::referenced_module_call_paths(program);
    resolve_and_build_deferred_environment_from_references(
        &referenced,
        surface,
        catalog,
        resolver,
        record,
        ctx,
    )
    .await
}

/// Variant used when another deferred-definition family shares the program's
/// receiver-call gather pass. Provider state and outcomes remain separate.
pub async fn resolve_and_build_deferred_environment_from_references(
    referenced: &BTreeSet<String>,
    surface: &LashlangSurface,
    catalog: &lash_core::ToolCatalog,
    resolver: Option<&SharedDeferredToolResolver>,
    record: &mut DeferredLink,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
) -> Result<LashlangHostEnvironment, DeferredResolutionError> {
    if referenced.is_empty() {
        return surface
            .host_environment(catalog)
            .map_err(|source| DeferredResolutionError::Ambient(Box::new(source)));
    }
    let recorded_paths = record
        .outcomes
        .keys()
        .filter(|path| referenced.contains(*path))
        .cloned()
        .collect::<BTreeSet<_>>();
    // The environment live classification built, kept for the final build
    // when that masks the same paths.
    let classified = std::sync::Mutex::new(None);
    let outcomes = journal_deferred_outcomes(
        referenced.clone(),
        || {
            // Retained outcomes own their exact paths. Classify live ambient
            // availability only after masking them, so later incompatible
            // schemas cannot preempt journal replay during environment build.
            let host_environment = surface.host_environment_masking(catalog, &recorded_paths)?;
            let ambient = referenced
                .iter()
                .filter(|path| already_provided(&host_environment, path))
                .cloned()
                .collect();
            *classified.lock_recover() = Some(host_environment);
            Ok(ambient)
        },
        resolver,
        record,
        ctx,
    )
    .await?;
    let masked_paths = outcomes.keys().cloned().collect::<BTreeSet<_>>();
    let classified = classified
        .lock_recover()
        .take()
        .filter(|_| masked_paths == recorded_paths);
    let mut host_environment = match classified {
        Some(host_environment) => host_environment,
        None => surface
            .host_environment_masking(catalog, &masked_paths)
            .map_err(|source| DeferredResolutionError::Ambient(Box::new(source)))?,
    };
    apply_deferred_outcomes(&mut host_environment, &outcomes, resolver, ctx)?;
    record.outcomes = outcomes;

    Ok(host_environment)
}

/// `gather → resolve → link`: [`resolve_and_fold_deferred`] then link. Used by
/// callers that do not maintain their own compile cache. `NotAvailable` (and no
/// resolver) leaves the symbol unresolved, surfacing a clean model-visible link
/// error.
pub async fn compile_with_deferred_resolution(
    workers: &lash_vm_client::service::Service,
    program: lashlang::Program,
    host_environment: LashlangHostEnvironment,
    resolver: Option<&SharedDeferredToolResolver>,
    record: &mut DeferredLink,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
) -> Result<lash_vm_client::service::CompiledModule, DeferredLinkError> {
    let host_environment =
        resolve_and_fold_deferred(&program, host_environment, resolver, record, ctx).await?;
    match workers
        .request_accounted(lash_vm_client::service::Request::LinkAst {
            source: String::new(),
            program,
            environment: host_environment,
        })
        .await
        .map_err(|error| DeferredLinkError::Worker(error.to_string()))?
    {
        lash_vm_client::service::Response::Module(module) => Ok(*module),
        lash_vm_client::service::Response::CompileRefused { error, .. } => Err(error.into()),
        other => Err(DeferredLinkError::Worker(format!(
            "unexpected compile response: {other:?}"
        ))),
    }
}

mod journal;
use journal::{apply_deferred_outcomes, journal_deferred_outcomes};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LashlangSurface, ToolBinding, ToolDefinitionBindingExt};

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
        .expect("valid declared tool schemas")
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
