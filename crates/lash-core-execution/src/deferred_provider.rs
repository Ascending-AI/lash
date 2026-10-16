//! The deferred tool provider: what resolves the call paths a run's surface
//! or a process's links do not offer (S6).
//!
//! A session or a process env registers at most one. It is a tool provider
//! under a declared key that advertises nothing and resolves on demand, and
//! each surface records its key. Resolution is read-only discovery: it is
//! repeat-safe and effect-free until its [`crate::DeferredMatch`] commits,
//! and a committed match is never resolved again.

use std::collections::BTreeMap;

use async_trait::async_trait;

use crate::plugin::OwnerConfigView;
use crate::{DeferredOutcome, ToolProvider};

/// Who one deferred resolution resolves for: the execution whose call paths
/// are resolved, the logical Run it belongs to, the capability refs that
/// Run's recorded shape names, and the configuration it runs under.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct DeferredResolveContext<'a> {
    /// The session frame or process whose call paths are resolved.
    /// The scoped address under which this resolver was registered.
    pub provider: &'a crate::plugin::ToolProviderHandle,
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
        provider: &'a crate::plugin::ToolProviderHandle,
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
    /// in the match's [`crate::ExecutionBinding`].
    async fn resolve(
        &self,
        cx: &DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> BTreeMap<String, DeferredOutcome>;
}
