//! Deployment declarations for removed session plugins, components and observers.
use super::*;

impl LashCoreBuilder {
    /// Declare a session plugin permanently removed from this deployment.
    /// Recorded sessions rebuild without it. New sessions naming it refuse
    /// typed; a recorded run still linking its tools or holding its state
    /// parks before resuming. Installing the same id or retiring it twice
    /// refuses the build. Process component ids are a separate registry.
    pub fn retire_session_plugin(mut self, id: lash_core::plugin::PluginId) -> Self {
        if let Err(error) = self.retired_session_plugins.retire(id.as_str(), (), false)
            && self.process_component_refusal.is_none()
        {
            self.process_component_refusal = Some(error);
        }
        self
    }

    /// Retire the session config-change observer `observer`, which this
    /// deployment uninstalled for good (FIG-6164): exactly that owner and
    /// key. A session whose head owes a config change to it is claimed by
    /// this core's node, and its delivery retires the observer's share as
    /// undeliverable, recorded in the head as a
    /// [`RetiredConfigDelivery`](crate::persistence::RetiredConfigDelivery)
    /// naming the change's revision, the observer and this core's lease
    /// owner. Its other observers are delivered as ever. The retirement
    /// resolves config-change deliveries only. The build refuses an identity
    /// that names no lifecycle observer, or one an installed plugin still
    /// registers.
    pub fn retire_config_observer(
        mut self,
        observer: lash_core::plugin::PluginCallbackIdentity,
    ) -> Self {
        self.retired_config_observers.push(observer);
        self
    }

    /// Retire the process component `declaration` names, which this
    /// deployment no longer installs (FIG-6296); `declaration` is the one
    /// it was installed under. A sealed env that names it still builds,
    /// without it, and its processes resume here; the env view lists it
    /// under [`ProcessAuthority::Engine`](crate::process::ProcessAuthority::Engine)'s
    /// `retired`. A new env that names it is refused with
    /// [`ProcessEnvRefusal::ComponentRetired`](lash_core::ProcessEnvRefusal::ComponentRetired).
    /// A process whose env links an effect to one of its providers, or
    /// that holds state it published, parks with the typed refusal instead
    /// of resuming without it. The build refuses a component that is also
    /// installed, or retired twice.
    pub fn retire_process_component(
        mut self,
        declaration: lash_core::plugin::ProcessComponentDeclaration,
    ) -> Self {
        if let Err(error) = self.process_components.retire(declaration)
            && self.process_component_refusal.is_none()
        {
            self.process_component_refusal = Some(error);
        }
        self
    }
}
