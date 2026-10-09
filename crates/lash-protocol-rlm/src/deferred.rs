//! Deferred tool resolution: tools a session grants a cell on demand.
//!
//! A host-provided [`DeferredToolResolver`] resolves call paths a cell
//! writes that the session's Tool Catalog does not offer into [`ToolGrant`]
//! values, which carry their Tool Execution Bindings, or reports
//! `NotAvailable`. The resolver resolves on demand only: it does not
//! enumerate, advertise or rank tools.
//!
//! A cell is lowered against the effects its host offers
//! ([`lash_vm_runtime::HostBoundary`]), so resolution runs before lowering:
//! `gather → journal → offer`. The call paths the cell's source writes that
//! no catalog tool answers are decided in one journaled step, keyed by the
//! cell's effect, and each granted tool is then offered as a kernel effect
//! under its binding's call path, with the signature its schemas state. A
//! redriven cell reuses its journaled decision without calling the resolver
//! again. The flat Tool Catalog is never mutated: a grant is the cell's.

/// version_surface = "coexist"
/// version_guard(items(DEFERRED_TOOL_RESOLUTION_PREFIX_VERSION, journal_deferred_outcomes))
const DEFERRED_TOOL_RESOLUTION_PREFIX_VERSION: &str = "deferred_tool_resolution:v2:";

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use lash_vm_runtime::{HostBoundary, ToolBindingError};

/// A host-authorized tool capability resolved for a deferred call path. It
/// carries the callable contract and identity (the tool definition) and the
/// host-owned Tool Execution Binding that routes a call to the backing
/// account, service, secret or remote executor.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ToolGrant {
    /// The callable contract and identity of the resolved tool.
    pub definition: lash_core::ToolDefinition,
    /// Optional registry source route authorized by the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// Host-owned routing authority, opaque to the runtime.
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

/// Outcome of resolving one deferred call path.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resolution {
    /// The call path resolved to a host-authorized tool.
    Resolved(Box<ToolGrant>),
    /// No tool is available for the call path: the cell's source names
    /// something the session does not offer.
    NotAvailable,
}

/// Who one deferred resolution resolves for: the execution whose cell is
/// lowered, the logical Run it belongs to, and the capability refs that
/// Run's recorded shape names.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct DeferredResolveContext<'a> {
    /// The session frame or process whose cell is lowered.
    pub owner: &'a lash_core::ExecutionOwner,
    /// The admitted logical Run; `None` for a process cell.
    pub run: Option<&'a lash_core::facade_support::TurnAddress>,
    /// The capability refs the Run's recorded shape names, by slot.
    pub capabilities: &'a BTreeMap<lash_core::SlotId, lash_core::CapabilityRef>,
}

impl<'a> DeferredResolveContext<'a> {
    pub fn new(
        owner: &'a lash_core::ExecutionOwner,
        run: Option<&'a lash_core::facade_support::TurnAddress>,
        capabilities: &'a BTreeMap<lash_core::SlotId, lash_core::CapabilityRef>,
    ) -> Self {
        Self {
            owner,
            run,
            capabilities,
        }
    }
}

/// Host-provided resolution of call paths the Tool Catalog does not offer.
#[async_trait]
pub trait DeferredToolResolver: Send + Sync {
    /// Resolve a deterministic batch of call paths (e.g. `web.fetch`) for
    /// the execution `cx` names. The batch holds every dotted path the
    /// cell's source calls that no catalog tool answers, so it may hold
    /// paths that are no tool at all: a path omitted from the returned map
    /// is recorded as [`Resolution::NotAvailable`], and entries for paths
    /// outside the batch are ignored.
    ///
    /// This is read-only discovery: it must not install routes or perform
    /// externally visible work. A precommit retry may call it again. Route
    /// mutation belongs in [`Self::install_recorded_grant`], after the
    /// outcome is journaled.
    async fn resolve(
        &self,
        cx: &DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> BTreeMap<String, Resolution>;

    /// Install the process-local execution route for a journaled grant.
    /// Route rehydration only: it makes no authorization decision and never
    /// widens the recorded grant. Idempotent.
    fn install_recorded_grant(
        &self,
        _path: &str,
        _grant: &ToolGrant,
    ) -> Result<(), RecordedGrantInstallError> {
        Ok(())
    }
}

/// Failure while restoring the process-local route of a journaled grant.
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

/// A cell could not restore or offer its already-decided authority.
#[derive(Debug, thiserror::Error)]
pub enum DeferredResolutionError {
    #[error("deferred resolution requires an admitted ExecCode effect address")]
    MissingLinkIdentity,
    #[error("failed to commit deferred resolution outcome: {0}")]
    Journal(#[source] lash_core::RuntimeEffectControllerError),
    #[error("journaled deferred resolution outcome is invalid: {0}")]
    InvalidJournaledOutcome(#[source] serde_json::Error),
    #[error("failed to restore recorded grant for `{path}`: {source}")]
    Install {
        path: String,
        #[source]
        source: RecordedGrantInstallError,
    },
    #[error("failed to offer recorded grant for `{path}`: {source}")]
    Offer {
        path: String,
        #[source]
        source: Box<ToolBindingError>,
    },
    #[error("the session cannot run the granted tool `{path}`: {source}")]
    Owner {
        path: String,
        #[source]
        source: lash_core::PluginError,
    },
}

impl DeferredResolutionError {
    /// Maps replay failures onto the runtime's bounded retry/terminal
    /// vocabulary without discarding the route-specific typed source.
    pub fn runtime_effect_error(&self) -> lash_core::RuntimeEffectControllerError {
        let code = match self {
            Self::Journal(error) => return error.clone(),
            Self::Install {
                source: RecordedGrantInstallError::Transient { .. },
                ..
            } => lash_core::RuntimeErrorCode::RuntimeStore,
            Self::MissingLinkIdentity
            | Self::InvalidJournaledOutcome(_)
            | Self::Install { .. }
            | Self::Offer { .. }
            | Self::Owner { .. } => lash_core::RuntimeErrorCode::ToolCatalogResolutionFailed,
        };
        lash_core::RuntimeEffectControllerError::new(code, self.to_string())
    }
}

/// A handle to the host's deferred resolver, optional because most hosts
/// ship no deferral.
pub type SharedDeferredToolResolver = Arc<dyn DeferredToolResolver>;

/// Every dotted path the source calls: `a.b.c(` as `a.b.c`. A scan of the
/// text, not a parse: it over-approximates (a method call on a value is a
/// path too), which costs the resolver a path it answers `NotAvailable`
/// for, and it never misses a call the dialect could lower to an effect,
/// because an effect is called by its dotted name.
pub(crate) fn called_paths(source: &str) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    let bytes = source.as_bytes();
    let ident_start = |byte: u8| byte == b'_' || byte.is_ascii_alphabetic();
    let ident_part = |byte: u8| byte == b'_' || byte.is_ascii_alphanumeric();
    let mut index = 0;
    while index < bytes.len() {
        if !ident_start(bytes[index])
            || (index > 0 && (ident_part(bytes[index - 1]) || bytes[index - 1] == b'.'))
        {
            index += 1;
            continue;
        }
        let start = index;
        let mut segments = 0;
        loop {
            while index < bytes.len() && ident_part(bytes[index]) {
                index += 1;
            }
            segments += 1;
            if index + 1 < bytes.len() && bytes[index] == b'.' && ident_start(bytes[index + 1]) {
                index += 1;
            } else {
                break;
            }
        }
        let end = index;
        let mut after = index;
        while after < bytes.len() && bytes[after].is_ascii_whitespace() {
            after += 1;
        }
        if segments > 1 && bytes.get(after) == Some(&b'(') {
            paths.insert(source[start..end].to_owned());
        }
    }
    paths
}

/// `gather → journal`: decides, in one journaled step keyed by the cell's
/// effect, every path of `candidates` (the call paths the source writes
/// that the catalog does not answer). A redrive is served the recorded
/// outcomes and never calls the resolver.
#[expect(
    clippy::result_large_err,
    reason = "the error carries the typed runtime error a nested effect records"
)]
pub(crate) async fn journal_deferred_outcomes(
    candidates: BTreeSet<String>,
    resolver: Option<&SharedDeferredToolResolver>,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
) -> Result<BTreeMap<String, Resolution>, DeferredResolutionError> {
    let address = ctx
        .parent_invocation()
        .and_then(lash_core::RuntimeInvocation::effect_address)
        .ok_or(DeferredResolutionError::MissingLinkIdentity)?;
    let effect_id = format!("{}:deferred-tool-resolution", address.replay_key);
    let operation = format!(
        "{DEFERRED_TOOL_RESOLUTION_PREFIX_VERSION}{}",
        serde_json::to_string(&candidates)
            .map_err(DeferredResolutionError::InvalidJournaledOutcome)?
    );
    let phase_context = ctx.clone();
    let resolver = resolver.cloned();
    let owner = ctx.owner().clone();
    let run = ctx.logical_run().cloned();
    let capabilities = ctx.run_capabilities().clone();
    let journaled = ctx
        .journaled_deferred_resolution_with(effect_id, operation, move || async move {
            let mut outcomes = BTreeMap::new();
            if let Some(resolver) = resolver.as_ref() {
                let paths = candidates.iter().map(String::as_str).collect::<Vec<_>>();
                let cx = DeferredResolveContext::new(&owner, run.as_ref(), &capabilities);
                let mut resolved = resolver.resolve(&cx, &paths).await;
                for path in paths {
                    outcomes.insert(
                        path.to_string(),
                        resolved.remove(path).unwrap_or(Resolution::NotAvailable),
                    );
                }
            }
            let _phase =
                phase_context.named_phase("rlm_lash_vm.deferred_resolve.after_resolver_return");
            serde_json::to_value(outcomes).map_err(|error| {
                lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::RecordEncodingFailed,
                    format!("failed to encode deferred resolution outcome: {error}"),
                )
            })
        })
        .await
        .map_err(DeferredResolutionError::Journal)?;
    {
        let _phase = ctx.named_phase("rlm_lash_vm.deferred_resolve.after_durable_record");
    }
    serde_json::from_value(journaled).map_err(DeferredResolutionError::InvalidJournaledOutcome)
}

/// `offer`: every granted tool of `outcomes` as an effect of `boundary`
/// under its binding's call path, its route installed, and the grant its
/// calls execute under.
#[expect(
    clippy::result_large_err,
    reason = "the error carries the typed runtime error a nested effect records"
)]
pub(crate) fn offer_deferred_grants(
    boundary: &mut HostBoundary,
    outcomes: &BTreeMap<String, Resolution>,
    resolver: Option<&SharedDeferredToolResolver>,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
) -> Result<BTreeMap<lash_core::ToolId, lash_core::ToolExecutionGrant>, DeferredResolutionError> {
    let mut grants = BTreeMap::new();
    for (path, resolution) in outcomes {
        let Resolution::Resolved(grant) = resolution else {
            continue;
        };
        let definition = &grant.definition;
        boundary
            .offer_bound_tool(
                &definition.manifest,
                definition.contract.input_schema.canonical(),
                definition.contract.output_schema.canonical(),
            )
            .map_err(|source| DeferredResolutionError::Offer {
                path: path.clone(),
                source: Box::new(source),
            })?;
        if let Some(resolver) = resolver {
            let _phase = ctx.named_phase("rlm_lash_vm.deferred_resolve.before_registration");
            resolver
                .install_recorded_grant(path, grant)
                .map_err(|source| DeferredResolutionError::Install {
                    path: path.clone(),
                    source,
                })?;
        }
        let owner = ctx
            .tool_execution_owner(&definition.manifest.id, grant.source_id.as_deref())
            .map_err(|source| DeferredResolutionError::Owner {
                path: path.clone(),
                source,
            })?;
        let mut execution_grant =
            lash_core::ToolExecutionGrant::from_definition(owner, definition.clone())
                .with_execution_binding(grant.execution_binding.clone());
        if let Some(source_id) = grant.source_id.as_deref() {
            execution_grant = execution_grant.with_source_id(source_id);
        }
        grants.insert(execution_grant.manifest().id.clone(), execution_grant);
    }
    Ok(grants)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gather step finds every dotted call and nothing that is not
    /// one: a bare call, a member read and a path inside a longer name are
    /// not candidates.
    #[test]
    fn the_gather_step_finds_dotted_calls() {
        let paths = called_paths(
            "const page = await web.fetch({url});\nprint(page.title);\nawait a.b.c (1); xs.map(f); web_fetch(1); q.r",
        );
        assert_eq!(
            paths.into_iter().collect::<Vec<_>>(),
            ["a.b.c", "web.fetch", "xs.map"]
        );
    }

    #[test]
    fn route_restore_failures_map_to_bounded_retry_policy() {
        let error = |source| {
            DeferredResolutionError::Install {
                path: "web.fetch".into(),
                source,
            }
            .runtime_effect_error()
        };
        let transient = error(RecordedGrantInstallError::transient("backend restarting"));
        let revoked = error(RecordedGrantInstallError::revoked("account disconnected"));
        assert_eq!(transient.code, lash_core::RuntimeErrorCode::RuntimeStore);
        assert!(transient.code.is_retryable());
        assert_eq!(
            revoked.code,
            lash_core::RuntimeErrorCode::ToolCatalogResolutionFailed
        );
        assert!(revoked.code.is_terminal());
    }
}
