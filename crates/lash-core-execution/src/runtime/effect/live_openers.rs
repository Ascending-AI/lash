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
use tokio_util::sync::CancellationToken;

use crate::EffectOpener;

/// The live tool-execution context one opener lends its group children.
///
/// An **owned** handle, deliberately: the borrowed
/// `RuntimeExecutionContext<'run>` cannot outlive the frame that made it, and a
/// child under [`LoserPolicy::RunToCompletion`](super::group::LoserPolicy::RunToCompletion)
/// must be able to outlive the caller that opened it. What is stored is the
/// opener's own dispatch context taken to `'static`, with its controller slots
/// lent the deployment host's owned controller for the opener's admitted
/// scope — never the opener's live handler-bound controller, which a Restate
/// handler cannot lend past its handler and which the driver's rebind
/// replaces anyway, so no child executes under it.
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
    /// The opener's own cooperative cancellation token.
    ///
    /// A child's body token is a child of this one, so a cooperative cancel
    /// signalled to the opener reaches work the child is running in the same
    /// process. For an opener whose turn control participates *locally* this
    /// is the only cancellation a child can honour — there is no durable
    /// address — and for a durable opener it is the same-process fast path
    /// beside the journaled gate the child's waits attach.
    cancellation: CancellationToken,
}

impl LiveOpenerContext {
    /// Captures an opener's dispatch context for the children it will open.
    ///
    /// `lent_controller` is what fills the captured context's controller
    /// slots: the deployment host's owned controller for the opener's admitted
    /// scope ([`EffectHost::scoped_static`](crate::EffectHost::scoped_static)),
    /// never the opener's live handler-bound controller — a Restate handler
    /// cannot lend its `ctx`-bound controller past its handler, and the
    /// group-child driver replaces both slots at its rebind anyway
    /// (`rebind_child_dispatch`), so no child ever executes under the lent
    /// controller. A host that hands out no owned controller answers
    /// `scoped_static` with `None` and the opener registers nothing.
    ///
    /// `cancellation` is the opener's own cooperative token — the turn's, or
    /// the process runner's — not a fresh one, because the child token the
    /// driver mints is a child of it and an orphan parent would make the
    /// child's cooperative cancel unsignalable.
    #[must_use]
    pub fn capture(
        dispatch: &crate::tool_dispatch::ToolDispatchContext<'_>,
        lent_controller: crate::ScopedEffectController<'static>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            dispatch: Arc::new(dispatch.lend_static(lent_controller)),
            cancellation,
        }
    }

    /// Captures the context with its event sender replaced.
    ///
    /// A turn's dispatch context carries the *per-phase* event channel, which
    /// the phase closes — and whose forwarder it awaits — when the phase ends.
    /// Registered as-is, the capture would pin that sender for the opener's
    /// whole life and the phase forwarder would wait on a channel that never
    /// closes. The sender lent to children is therefore a channel whose
    /// lifetime is the registration's, owned by whoever registered the opener.
    ///
    /// `lent_controller` is the same lend [`Self::capture`] takes.
    #[must_use]
    pub fn capture_with_event_sender(
        dispatch: &crate::tool_dispatch::ToolDispatchContext<'_>,
        lent_controller: crate::ScopedEffectController<'static>,
        event_tx: tokio::sync::mpsc::Sender<crate::SessionStreamEvent>,
        cancellation: CancellationToken,
    ) -> Self {
        let mut dispatch = dispatch.lend_static(lent_controller);
        dispatch.event_tx = event_tx;
        Self {
            dispatch: Arc::new(dispatch),
            cancellation,
        }
    }

    /// Lends the turn's `TurnActivity` channel to the captured dispatch.
    ///
    /// Same per-phase-sender reasoning as the event sender
    /// [`Self::capture_with_event_sender`] replaces: the channel lent here is
    /// the registration-owned one the opener's forwarder feeds, so a child's
    /// nested calls surface their `ToolCallStarted`/`ToolCallCompleted`
    /// activities on the turn stream after the phase that opened them has
    /// ended.
    #[must_use]
    pub fn with_turn_activity_sender(
        mut self,
        turn_activity_tx: tokio::sync::mpsc::Sender<crate::TurnActivity>,
    ) -> Self {
        if let Some(dispatch) = Arc::get_mut(&mut self.dispatch) {
            dispatch.turn_activity_tx = Some(turn_activity_tx);
        }
        self
    }

    /// The opener's dispatch context, for the driver to rebind against one
    /// child's recorded request.
    #[must_use]
    pub fn dispatch(&self) -> &Arc<crate::tool_dispatch::ToolDispatchContext<'static>> {
        &self.dispatch
    }

    /// The opener's cooperative cancellation token, the parent of the child's
    /// own body token.
    #[must_use]
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
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
    openers: Mutex<HashMap<EffectOpener, LiveOpenerEntry>>,
    /// Monotonic, so a re-registration can be told from the registration it
    /// replaced. Without it a redriven opener's predecessor guard — which may
    /// drop at any moment, since the old worker is winding down concurrently —
    /// would deregister the newcomer and silently strand its children.
    next_generation: Mutex<u64>,
}

/// One registered opener: its generation, the context it lends, and the
/// cancellation that fires the moment this entry stops being the live one —
/// the guard dropped or a redrive superseding it.
///
/// The token exists because a registration can own live work that must end
/// with it, not with the last borrower: the turn path runs an event forwarder
/// whose sender would otherwise hold the turn's stream open past its drain.
/// Children never see it — [`context_for`](LiveOpenerRegistry::context_for)
/// hands out the context alone — so a child outliving its opener cannot keep
/// the registration's work alive.
struct LiveOpenerEntry {
    generation: u64,
    context: LiveOpenerContext,
    /// Drops with the entry, cancelling the token `register` handed the caller.
    _ended: tokio_util::sync::DropGuard,
}

impl LiveOpenerRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `opener` as live here, returning the guard that deregisters it
    /// and a token cancelled the moment this entry stops being the live one.
    ///
    /// The guard is the deregistration, so an opener cannot be left registered
    /// by an early return, a cancelled future or an unwind — the same reason
    /// `IntentDrainGuard` discharges from `Drop`. Note that this is a
    /// process-local lifetime and nothing durable: dropping the guard says
    /// "this worker is no longer running that opener", never "that opener is
    /// closed", which is a durable fact §7 owns.
    ///
    /// The token fires on either end — guard drop or a re-registration
    /// superseding the entry — so work owned by the registration (the turn
    /// path's child-event forwarder) ends with it rather than lingering for
    /// the last borrowed context to drop.
    ///
    /// **A re-registration of the same opener replaces the entry**, which is
    /// what a redrive is: the new worker's context supersedes a stale one, and
    /// the previous guard becomes inert rather than removing the newcomer.
    pub fn register(
        self: &Arc<Self>,
        opener: EffectOpener,
        context: LiveOpenerContext,
    ) -> (LiveOpenerGuard, CancellationToken) {
        let generation = {
            let mut next = self.next_generation.lock_recover();
            *next = next.saturating_add(1);
            *next
        };
        let ended = CancellationToken::new();
        self.openers.lock_recover().insert(
            opener.clone(),
            LiveOpenerEntry {
                generation,
                context,
                _ended: ended.clone().drop_guard(),
            },
        );
        (
            LiveOpenerGuard {
                registry: Arc::clone(self),
                opener,
                generation,
            },
            ended,
        )
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
            .map(|entry| entry.context.clone())
    }

    /// The generation of `opener`'s live registration, or `None` when the
    /// opener is not live here.
    ///
    /// A re-registration of the same opener value is a new generation, so a
    /// caller holding a generation from an earlier resolution can tell "the
    /// registration that produced this is still the live one" from "a redrive
    /// re-registered it since" — the distinction a reopened group needs to
    /// know whether its recorded state still belongs to the live incarnation.
    #[must_use]
    pub fn generation_of(&self, opener: &EffectOpener) -> Option<u64> {
        self.openers
            .lock_recover()
            .get(opener)
            .map(|entry| entry.generation)
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

    /// Removes `opener` only if it is still the registration `generation`
    /// named, so a superseded guard cannot evict its successor.
    fn deregister(&self, opener: &EffectOpener, generation: u64) {
        let mut openers = self.openers.lock_recover();
        if openers
            .get(opener)
            .is_some_and(|entry| entry.generation == generation)
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
