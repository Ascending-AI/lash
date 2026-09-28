use super::RestateEffectHostController;
use std::sync::Arc;

use lash_core::{
    GroupExecutors, RuntimeEffectEnvelope, RuntimeEffectGroup, RuntimeEffectLocalExecutor,
    RuntimeError, ScopedEffectController,
};

/// The deployment host's registered resolver, read at call time.
///
/// The endpoint's `EffectGroupDispatch` holds this from construction: it must
/// not capture a resolver snapshot, because the one registration a
/// `ToolChildHost` install performs can land after the services were built.
pub(super) struct RestateHostGroupExecutors {
    pub(super) controller: Arc<RestateEffectHostController>,
}

impl GroupExecutors for RestateHostGroupExecutors {
    fn pin_group(&self, group: &RuntimeEffectGroup) {
        if let Some(executors) = self.controller.group_executors.get() {
            executors.pin_group(group);
        }
    }

    fn release_child(&self, envelope: &RuntimeEffectEnvelope) {
        if let Some(executors) = self.controller.group_executors.get() {
            executors.release_child(envelope);
        }
    }

    fn release_group(&self, group_key: &str) {
        if let Some(executors) = self.controller.group_executors.get() {
            executors.release_group(group_key);
        }
    }

    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        self.controller
            .group_executors
            .get()?
            .executor_for(envelope)
    }

    fn routes(&self, envelope: &RuntimeEffectEnvelope) -> bool {
        self.controller
            .group_executors
            .get()
            .is_some_and(|executors| executors.routes(envelope))
    }

    /// The registered resolver's routing, read at call time like every
    /// other answer here; with nothing registered there is no host stack to
    /// route through.
    fn route_handler_child_controller<'run>(
        &self,
        controller: ScopedEffectController<'run>,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        match self.controller.group_executors.get() {
            Some(executors) => executors.route_handler_child_controller(controller),
            None => Ok(controller),
        }
    }
}
