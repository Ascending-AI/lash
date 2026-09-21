//! The live openers this process is running, and the tool-execution context
//! each one lends its group children (ADR 0099 §3, FIG-2266).
//!
//! # Why a registry exists at all
//!
//! A tool child of an effect group is a *replayable invocation driver* (§2)
//! that runs with no caller in scope: on a retry, on a drain, and on a fresh
//! handler execution there is nothing borrowed to reach for. [`ToolChildRequest`]
//! answers most of what the driver needs, because §3 makes the recorded facts
//! authoritative — the call, the admission, the attempt identity, the opener,
//! the environment reference and the completion routing all come out of the
//! journal.
//!
//! What the request deliberately does **not** answer is the live half. §3 is
//! explicit that "`RuntimeExecutionContext` is never serialized and there is no
//! second environment store … Semantic completion facts travel; live channels do
//! not." A turn's plugin inputs, its provider handle, its stream sender and its
//! direct-completion client are live state; recording them would pin a recovered
//! child to a worker that no longer exists.
//!
//! On the **in-process tiers** that live half is not gone, it is simply not in
//! the journal: the opener is running in this very process, and it already holds
//! exactly the context its children need. This registry is how a child finds it,
//! and the rule that makes it sound is the narrow one — **a tool child executes
//! here only where its opener is live here**.
//!
//! # Keyed by the opener value, never by a rendered string
//!
//! The key is an [`EffectOpener`], compared as a value. Its own documentation
//! says why a rendering will not do: a turn's scope identity is free-form text
//! that can contain exactly the `{process_id}#{incarnation}` a process opener
//! renders to, so an untagged string admits two distinct openers that mint one
//! identity — the aliasing §1 refuses. A process opener therefore carries its
//! incarnation here as everywhere else, and a process re-registered under the
//! same name is a *different* key, which is what stops it inheriting its
//! predecessor's children.
//!
//! # One owner per opener kind
//!
//! Registration is not something any holder of a context may do; each opener
//! kind has exactly one owner, so the lifetime of an entry is a property of one
//! code path rather than of whoever happened to call last.
//!
//! * **A turn opener** is registered by the turn path when the turn starts and
//!   deregistered when that opener reaches *settled*. On today's path settled is
//!   turn end, because the durable live→closing transition of ADR 0099 §7 does
//!   not exist yet; when FIG-3410 lands it, the deregistration moves to the end
//!   of finalization and this comment is the thing to update. Until then a
//!   child whose opener's turn has ended is not runnable here, which is the
//!   conservative direction: it stays accepted for recovery rather than running
//!   under a context that is finishing.
//! * **A process opener** is registered at process-incarnation start and
//!   deregistered at its terminal.
//!
//! **A redriven opener re-registers**, and that is precisely how §0's "an
//! accepted child is *recovered* while its opener lives" reaches its children on
//! these tiers: the new worker registers the same [`EffectOpener`] value, and
//! the children it left behind become runnable again without anything being
//! re-decided.
//!
//! # An unregistered opener is a routing fact, not a failure
//!
//! [`context_for`](LiveOpenerRegistry::context_for) answering `None` means "not
//! mine", exactly as [`GroupExecutors::executor_for`] answering `None` does, and
//! it is resolved the same way: the child is **not run here and not failed**. It
//! stays accepted, and some process whose opener is live — or a later
//! incarnation of this one — runs it. Failing instead would be the worse error
//! by far, because it would convert "this worker cannot reach that opener" into
//! a terminal the journal keeps forever.
//!
//! # No global state
//!
//! The registry hangs off the same host object the resolver is registered on and
//! is reached through an `Arc`. There is no static: two hosts in one process —
//! which the conformance suites build routinely — must not see each other's
//! openers, or a child would run against a deployment that never admitted it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use lash_sansio::sync::MutexExt;

use super::executor::RuntimeEffectControllerError;
use crate::EffectOpener;

/// The live tool-execution context one opener lends its group children.
///
/// An **owned** handle, deliberately: the borrowed
/// `RuntimeExecutionContext<'run>` cannot outlive the frame that made it, and a
/// child under [`LoserPolicy::RunToCompletion`](super::group::LoserPolicy::RunToCompletion)
/// must be able to outlive the caller that opened it. What is stored is the
/// opener's own dispatch context taken to `'static`.
///
/// # Why the whole dispatch context rather than a hand-picked subset
///
/// The driver needs the fields that are neither recorded in the request nor
/// derivable from deployment wiring — the plugin session, the turn context, the
/// stream sender, the direct-completion client. A struct naming just those
/// would be a second list to keep in step with `ToolDispatchContext`'s 24
/// fields, and the first thing to drift when a field is added. Lending the
/// opener's own context and having the driver **rebind** the recorded fields
/// keeps one list, and makes the rebinding explicit and reviewable at the one
/// place it happens.
///
/// This is not a `RuntimeExecutionContext` and cannot become one: it carries no
/// process-env store, no chronological projection and no protocol extension,
/// because a child reconstructs those from its own request and host rather than
/// inheriting the opener's.
#[derive(Clone)]
pub struct LiveOpenerContext {
    dispatch: Arc<crate::tool_dispatch::ToolDispatchContext<'static>>,
}

impl LiveOpenerContext {
    /// Captures an opener's dispatch context for the children it will open.
    ///
    /// Returns `None` when the context cannot be taken to `'static`, which is
    /// the same condition
    /// [`ToolDispatchContext::to_static`](crate::tool_dispatch::ToolDispatchContext::to_static)
    /// already reports: a controller or completion client that is borrowed for
    /// one frame cannot lend itself to a child that outlives it. A caller that
    /// meets it must not register, because a half-captured opener would be a
    /// registry entry whose children could never actually run.
    #[must_use]
    pub fn capture(dispatch: &crate::tool_dispatch::ToolDispatchContext<'_>) -> Option<Self> {
        dispatch.to_static().map(|dispatch| Self {
            dispatch: Arc::new(dispatch),
        })
    }

    /// The opener's dispatch context, for the driver to rebind against one
    /// child's recorded request.
    #[must_use]
    pub fn dispatch(&self) -> &Arc<crate::tool_dispatch::ToolDispatchContext<'static>> {
        &self.dispatch
    }
}

impl std::fmt::Debug for LiveOpenerContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("LiveOpenerContext").finish()
    }
}

/// The openers running in this host, and the context each lends its children.
///
/// See the module documentation for the ownership rules. The map is small by
/// construction — one entry per opener this host is currently running — and is
/// read once per child resolution, so a plain mutex is the right primitive.
#[derive(Default)]
pub struct LiveOpenerRegistry {
    openers: Mutex<HashMap<EffectOpener, (u64, LiveOpenerContext)>>,
    /// Monotonic, so a re-registration can be told from the registration it
    /// replaced. Without it a redriven opener's predecessor guard — which may
    /// drop at any moment, since the old worker is winding down concurrently —
    /// would deregister the newcomer and silently strand its children.
    next_generation: Mutex<u64>,
}

impl LiveOpenerRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `opener` as live here, returning the guard that deregisters it.
    ///
    /// The guard is the deregistration, so an opener cannot be left registered
    /// by an early return, a cancelled future or an unwind — the same reason
    /// `IntentDrainGuard` discharges from `Drop`. Note that this is a
    /// process-local lifetime and nothing durable: dropping the guard says
    /// "this worker is no longer running that opener", never "that opener is
    /// closed", which is a durable fact §7 owns.
    ///
    /// **A re-registration of the same opener replaces the entry**, which is
    /// what a redrive is: the new worker's context supersedes a stale one, and
    /// the previous guard becomes inert rather than removing the newcomer.
    pub fn register(
        self: &Arc<Self>,
        opener: EffectOpener,
        context: LiveOpenerContext,
    ) -> LiveOpenerGuard {
        let generation = {
            let mut next = self.next_generation.lock_recover();
            *next = next.saturating_add(1);
            *next
        };
        self.openers
            .lock_recover()
            .insert(opener.clone(), (generation, context));
        LiveOpenerGuard {
            registry: Arc::clone(self),
            opener,
            generation,
        }
    }

    /// The live context for `opener`, or `None` when this host is not running
    /// it.
    ///
    /// `None` is a routing fact. See the module documentation: the caller
    /// leaves the child accepted rather than failing it.
    #[must_use]
    pub fn context_for(&self, opener: &EffectOpener) -> Option<LiveOpenerContext> {
        self.openers
            .lock_recover()
            .get(opener)
            .map(|(_, context)| context.clone())
    }

    /// Whether `opener` is live in this host.
    #[must_use]
    pub fn is_live(&self, opener: &EffectOpener) -> bool {
        self.openers.lock_recover().contains_key(opener)
    }

    /// How many openers this host is currently running.
    #[must_use]
    pub fn len(&self) -> usize {
        self.openers.lock_recover().len()
    }

    /// Whether this host is running no openers at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The typed refusal for a child whose opener is live but whose context
    /// cannot serve it.
    ///
    /// Distinct from the `None` above on purpose: "this host does not run that
    /// opener" is routing, while "this host runs it and still cannot build the
    /// child's context" is a defect the operator has to see.
    pub(crate) fn context_unavailable(opener: &EffectOpener) -> RuntimeEffectControllerError {
        RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
            format!(
                "opener {} is live in this host but lent no usable tool-execution context",
                opener.render()
            ),
        )
    }

    /// Removes `opener` only if it is still the registration `generation`
    /// named, so a superseded guard cannot evict its successor.
    fn deregister(&self, opener: &EffectOpener, generation: u64) {
        let mut openers = self.openers.lock_recover();
        if openers
            .get(opener)
            .is_some_and(|(current, _)| *current == generation)
        {
            openers.remove(opener);
        }
    }
}

impl std::fmt::Debug for LiveOpenerRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveOpenerRegistry")
            .field("openers", &self.len())
            .finish()
    }
}

/// One opener's registration, deregistered on drop.
///
/// Held by the owner named in the module documentation — the turn path for a
/// turn opener, the process path for a process incarnation — and by nobody
/// else.
pub struct LiveOpenerGuard {
    registry: Arc<LiveOpenerRegistry>,
    opener: EffectOpener,
    generation: u64,
}

impl LiveOpenerGuard {
    /// The opener this guard keeps live.
    #[must_use]
    pub fn opener(&self) -> &EffectOpener {
        &self.opener
    }
}

impl Drop for LiveOpenerGuard {
    fn drop(&mut self) {
        self.registry.deregister(&self.opener, self.generation);
    }
}

impl std::fmt::Debug for LiveOpenerGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveOpenerGuard")
            .field("opener", &self.opener)
            .finish()
    }
}

#[cfg(test)]
#[path = "live_openers/tests.rs"]
mod tests;
