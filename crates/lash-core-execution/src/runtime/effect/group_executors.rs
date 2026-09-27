use super::envelope::RuntimeEffectEnvelope;
use super::executor::RuntimeEffectLocalExecutor;

/// Resolves a recorded group child to the executor owned by this host.
pub trait GroupExecutors: Send + Sync {
    /// The executor for `envelope`, or `None` when this host cannot run it.
    ///
    /// Called once per child per resolution, with the envelope as the journal
    /// recorded it — or, at open, as the group carries it — including its
    /// [`EffectGroupMembership`](super::group::EffectGroupMembership), so a host
    /// can route on the group as well as on the command.
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>>;

    /// Whether this host runs `envelope`'s child at all, wherever the child's
    /// opener is live.
    ///
    /// [`executor_for`](Self::executor_for) answers for *this process*: a tool
    /// child whose opener is live in another worker is `None` here and runs
    /// there. The opener's own process answers a first open with
    /// `executor_for`, which is what refuses an absent, foreign or superseded
    /// opener (ADR 0099 §1). A check made from an endpoint handler asks this
    /// instead: Restate routes a group's dispatcher, and a handler-driven
    /// open's preflight, to whichever worker takes the call, and reading "the
    /// opener is live elsewhere" as "no executor" would refuse a group the
    /// opener's own worker runs (FIG-3630). The default is `executor_for`'s
    /// answer, which is right for every resolver whose routing does not depend
    /// on which process asks.
    fn routes(&self, envelope: &RuntimeEffectEnvelope) -> bool {
        self.executor_for(envelope).is_some()
    }

    /// The generation of `opener`'s live registration, when this host tracks
    /// opener registrations at all.
    ///
    /// A controller that reopens a group compares the generation the group
    /// was opened under against the live one: a same-value re-registration is
    /// a new incarnation, and serving the reopened caller the superseded
    /// registration's recorded state would stamp a dead context's answers
    /// under the new incarnation's name. `None` means either the opener is
    /// not live here or the resolver keeps no registry — an unversioned
    /// answer the controller must read as "the recorded state stands".
    fn live_generation(&self, opener: &crate::EffectOpener) -> Option<u64> {
        let _ = opener;
        None
    }

    /// A group child's controller that an engine handler minted from the
    /// child invocation's own context, routed through the stack of the host
    /// this resolver runs children for
    /// ([`EffectHost::route_handler_child_controller`](super::executor::EffectHost::route_handler_child_controller)).
    ///
    /// Every kind of child a handler-driven engine runs (a tool child's
    /// driver, a timer, a durable wait) is routed here once, before its
    /// first effect, so a layer over that host sees the child's effects as
    /// it sees the effects of controllers the host lends itself. The default
    /// is the controller unchanged: a resolver that belongs to no layered
    /// host has no stack to add.
    fn route_handler_child_controller<'run>(
        &self,
        controller: crate::ScopedEffectController<'run>,
    ) -> Result<crate::ScopedEffectController<'run>, crate::RuntimeError> {
        Ok(controller)
    }
}
