//! The deferred tool provider: what resolves the call paths a run's surface
//! or a process's links do not offer (S6).
//!
//! A session or a process env registers at most one. It is a tool provider
//! under a declared key that advertises nothing and resolves on demand, and
//! each surface records its key. Resolution is read-only discovery: it is
//! repeat-safe and effect-free until its [`crate::DeferredMatch`] commits,
//! and a committed match is never resolved again.
//!
//! A process's call whose target is known only at run time is the call of a
//! host-named tool its deferred provider's registration grants dispatch
//! ([`DispatchGrant`]): the host's pure transform names the call path and
//! its arguments, the provider resolves the path once, and the matched tool
//! is called with the arguments (P6).

use std::collections::BTreeMap;

use async_trait::async_trait;

use crate::plugin::OwnerConfigView;
use crate::{DeferredOutcome, ToolProvider};

/// A deferred provider's typed host registration, with the dispatch grants
/// it holds. Its resolution policy is sealed into each process environment
/// with the grant link it produces; takeover serves that recorded link even
/// when this registration changes.
#[derive(Clone)]
pub struct DeferredToolProviderRegistration {
    pub(crate) provider: std::sync::Arc<dyn DeferredToolProvider>,
    resolution_policy: lash_kernel_value::providers::DeferredResolutionPolicy,
    pub(crate) grants: BTreeMap<lash_vocabulary::tool_contract::ToolId, DispatchGrant>,
}

impl DeferredToolProviderRegistration {
    /// Register with the documented 60-second, three-attempt default and no
    /// dispatch grant.
    pub fn new(provider: std::sync::Arc<dyn DeferredToolProvider>) -> Self {
        Self {
            provider,
            resolution_policy: lash_kernel_value::providers::DeferredResolutionPolicy::default(),
            grants: BTreeMap::new(),
        }
    }

    /// Select the contract new process environments record for discovery.
    #[must_use]
    pub fn with_resolution_policy(
        mut self,
        policy: lash_kernel_value::providers::DeferredResolutionPolicy,
    ) -> Self {
        self.resolution_policy = policy;
        self
    }

    /// Grant `grant`'s host-named tool dispatch through this provider: a
    /// call of it runs the grant's transform, resolves the path it names
    /// here, and calls the matched tool with the transform's arguments.
    ///
    /// # Errors
    ///
    /// [`DispatchGrantRefusal::Duplicate`] when this registration already
    /// grants a tool with that id.
    pub fn with_dispatch_grant(
        mut self,
        grant: DispatchGrant,
    ) -> Result<Self, DispatchGrantRefusal> {
        let tool = grant.definition.manifest.id.clone();
        if self.grants.contains_key(&tool) {
            return Err(DispatchGrantRefusal::Duplicate { tool });
        }
        self.grants.insert(tool, grant);
        Ok(self)
    }

    /// The link of `tool`'s dispatch grant through this registration's
    /// provider, registered under `provider`, with its resolution policy
    /// captured now. Pass it to `ProcessEnvBuilder::link` at the effect
    /// the granted tool's operation names; later registrations cannot
    /// change it.
    ///
    /// # Errors
    ///
    /// [`DispatchGrantRefusal::NotGranted`] when this registration grants
    /// no tool `tool`: only a granted tool dispatches.
    pub fn dispatch_link(
        &self,
        provider: crate::ProviderKey,
        tool: &crate::ToolId,
    ) -> Result<crate::OperationRoute, DispatchGrantRefusal> {
        let grant = self
            .grants
            .get(tool)
            .ok_or_else(|| DispatchGrantRefusal::NotGranted { tool: tool.clone() })?;
        Ok(crate::OperationRoute::dispatch(
            provider,
            grant.definition.clone(),
            self.resolution_policy,
        ))
    }
}

/// The call a granted tool's transform names: the call path its deferred
/// provider resolves, and the arguments the matched tool is called with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchTarget {
    pub path: String,
    pub args: serde_json::Value,
}

/// A granted tool's transform refused the tool's input: nothing was
/// resolved.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{reason}")]
pub struct TransformRefusal {
    pub reason: String,
}

/// A host's transform of a granted tool's input into the call it names.
///
/// It must be deterministic and pure: lash never journals its answer, and
/// runs it again wherever the answer is needed, the call of the matched tool
/// after a crash or a takeover included. A transform that answers one input
/// with two call paths is a host bug, which lash detects as a divergence
/// from the committed match and refuses typed (`dispatch_diverged`).
pub type DispatchTransform = std::sync::Arc<
    dyn Fn(&serde_json::Value) -> Result<DispatchTarget, TransformRefusal> + Send + Sync,
>;

/// A host-named tool granted dispatch: its definition, whose name, schema
/// and description its callers see, and the transform of its input into the
/// call it names.
#[derive(Clone)]
pub struct DispatchGrant {
    pub(crate) definition: lash_vocabulary::tool_contract::ToolDefinition,
    pub(crate) transform: DispatchTransform,
}

impl DispatchGrant {
    /// Grant `definition` dispatch through `transform`, a deterministic,
    /// pure function ([`DispatchTransform`]).
    pub fn new(
        definition: lash_vocabulary::tool_contract::ToolDefinition,
        transform: impl Fn(&serde_json::Value) -> Result<DispatchTarget, TransformRefusal>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            definition,
            transform: std::sync::Arc::new(transform),
        }
    }
}

/// A refused dispatch grant.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DispatchGrantRefusal {
    /// The registration grants no such tool: only a granted tool dispatches.
    #[error("tool `{tool}` holds no dispatch grant")]
    NotGranted {
        tool: lash_vocabulary::tool_contract::ToolId,
    },
    /// The registration already grants a tool with this id.
    #[error("tool `{tool}` is granted dispatch twice")]
    Duplicate {
        tool: lash_vocabulary::tool_contract::ToolId,
    },
}

/// Why a granted tool's call dispatches nothing: a typed tool failure its
/// caller may catch. Nothing was resolved, or the matched tool did not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchRefusal {
    /// This node's deferred provider registration does not grant the
    /// linked tool: only a granted tool dispatches.
    NotGranted {
        effect: String,
        tool: lash_vocabulary::tool_contract::ToolId,
    },
    /// The transform or provider alias entered a statically linked package root.
    PackageNamespace { path: String },
    /// The grant's transform refused the tool's input.
    InputRefused {
        tool: lash_vocabulary::tool_contract::ToolId,
        reason: String,
    },
    /// The grant's transform, run again for the call of the matched tool,
    /// named another call path than the committed match resolved.
    Diverged {
        tool: lash_vocabulary::tool_contract::ToolId,
        committed: String,
        recomputed: String,
    },
}

impl DispatchRefusal {
    /// The typed tool failure the call settles with.
    #[must_use]
    pub fn failure(&self) -> crate::ToolFailure {
        match self {
            Self::PackageNamespace { path } => crate::ToolFailure::runtime(
                crate::ToolFailureClass::InvalidRequest,
                "package_namespace",
                format!("computed calls cannot enter the linked package namespace `{path}`"),
            )
            .with_cause(crate::ToolFailureCause::PackageNamespace { path: path.clone() }),
            Self::NotGranted { effect, tool } => crate::ToolFailure::runtime(
                crate::ToolFailureClass::PermissionDenied,
                "dispatch_not_granted",
                format!(
                    "the effect `{effect}` is linked to tool `{tool}` as a dispatch grant this \
                     node's deferred provider does not grant; nothing was resolved"
                ),
            )
            .with_cause(crate::ToolFailureCause::DispatchNotGranted {
                effect: effect.clone(),
                tool: tool.clone(),
            }),
            Self::InputRefused { tool, reason } => crate::ToolFailure::runtime(
                crate::ToolFailureClass::InvalidRequest,
                "dispatch_input_refused",
                format!("tool `{tool}` names no call for its input: {reason}"),
            )
            .with_cause(crate::ToolFailureCause::DispatchInputRefused {
                tool: tool.clone(),
                reason: reason.clone(),
            }),
            Self::Diverged {
                tool,
                committed,
                recomputed,
            } => crate::ToolFailure::runtime(
                crate::ToolFailureClass::Internal,
                "dispatch_diverged",
                format!(
                    "tool `{tool}`'s transform named `{recomputed}` for the input whose \
                     committed match resolved `{committed}`: its transform is not \
                     deterministic; the matched tool did not run"
                ),
            )
            .with_cause(crate::ToolFailureCause::DispatchDiverged {
                tool: tool.clone(),
                committed: committed.clone(),
                recomputed: recomputed.clone(),
            }),
        }
    }
}

/// Who one deferred resolution resolves for: the execution whose call paths
/// are resolved, the logical Run it belongs to, the capability refs that
/// Run's recorded shape names, and the configuration it runs under.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct DeferredResolveContext<'a> {
    /// The session frame or process whose call paths are resolved.
    /// The scoped address under which this resolver was registered.
    pub provider: &'a lash_vocabulary::providers::ToolProviderHandle,
    pub owner: &'a crate::ExecutionOwner,
    /// The admitted logical Run; `None` for a process.
    pub run: Option<&'a crate::runtime::TurnAddress>,
    /// The capability refs the Run's recorded shape names, by slot.
    pub capabilities: &'a BTreeMap<crate::SlotId, crate::CapabilityRef>,
    /// The run's resolved configuration, or the process env's.
    pub config: &'a OwnerConfigView,
}

impl<'a> DeferredResolveContext<'a> {
    pub fn new(
        provider: &'a lash_vocabulary::providers::ToolProviderHandle,
        owner: &'a crate::ExecutionOwner,
        run: Option<&'a crate::runtime::TurnAddress>,
        capabilities: &'a BTreeMap<crate::SlotId, crate::CapabilityRef>,
        config: &'a OwnerConfigView,
    ) -> Self {
        Self {
            provider,
            owner,
            run,
            capabilities,
            config,
        }
    }
}

/// A tool provider that resolves call paths no surface offers. It
/// advertises nothing; a resolved match names the provider that executes
/// it, which may be this one.
#[async_trait]
pub trait DeferredToolProvider: ToolProvider {
    /// Searchable tool names and their resolver call paths under this run's
    /// configuration and capabilities. Names only: this neither resolves a
    /// definition nor grants authority to call it. Providers without a
    /// searchable catalogue return an empty map. This is a pure, deterministic
    /// projection: implementations perform no I/O or authorization.
    fn searchable_names(&self, _cx: &DeferredResolveContext<'_>) -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    /// Resolve a deterministic batch of call paths. A path omitted from the
    /// returned map is recorded as [`DeferredOutcome::Unavailable`], and
    /// entries for paths outside the batch are ignored. Host notes and refusal
    /// reasons are recorded verbatim.
    ///
    /// This is read-only discovery: it must not install routes or perform
    /// externally visible work, since a resolution that has not committed
    /// may run again. What a resolved tool needs to route its calls travels
    /// in the match's [`lash_vocabulary::providers::ExecutionBinding`].
    async fn resolve(
        &self,
        cx: &DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> BTreeMap<String, DeferredOutcome>;
}
