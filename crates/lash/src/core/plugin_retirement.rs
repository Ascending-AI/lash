//! Deployment declarations for removed session plugins, components and observers.
use super::*;

impl LashCoreBuilder {
    /// Declare a session plugin permanently removed from this deployment.
    /// Recorded sessions rebuild without it. New sessions naming it refuse
    /// typed; a recorded run still linking its tools or holding its state
    /// parks before resuming. Installing the same id or retiring it twice
    /// refuses the build. Process component ids are a separate registry.
    ///
    /// Rebuild checks no required installed-plugin roster. Durable references
    /// can occur in the session's config (the protocol id and owner namespaces),
    /// its prompt plan, plugin-state namespaces and publication batches, and a
    /// run's recorded config, owner options and tool surface (including
    /// deferred-provider identities). Checkpoints
    /// and call records can also name callback owners in hook, preparation and
    /// presentation decisions, stream state, and owed callbacks; actor claim
    /// requirements carry those identities too. Plugin transitions retain writer
    /// admissions and namespace postimages; prompt snapshots retain section
    /// and wrapper owners, and messages/events retain plugin provenance.
    /// Config-change deliveries name their observers, including completed
    /// retirement receipts; an undelivered
    /// observer needs its own [`Self::retire_config_observer`] declaration.
    ///
    /// A plugin that leaves none of these references needs no retirement: a
    /// session created while it was installed already rebuilds without it.
    /// In particular, an empty, configless plugin needs no retirement: empty
    /// state namespaces and recorded transition writer admissions do not
    /// require its factory to be installed.
    /// `SessionSpec::plugin_options` accepts only registered config owners;
    /// adding an empty option for a configless plugin is refused as
    /// `ConfigRefusalReason::UnknownOwner`, even while it is installed.
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
