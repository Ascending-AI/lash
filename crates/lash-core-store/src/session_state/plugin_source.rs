//! The plugin-side source a session state captures plugin namespaces from.

/// The plugin-side facts durable session state refreshes itself from.
///
/// `lash-core`'s `PluginSession` is the sole implementor; the trait exists so
/// the durable state struct does not need the plugin host to describe itself.
pub trait SessionPluginStateSource {
    /// Recorded transition receipt and current native view, without conversion.
    fn capture_plugin_admission(
        &self,
        config: &crate::PluginConfig,
    ) -> Result<Option<std::sync::Arc<[u8]>>, crate::RuntimeError>;

    /// Current tool-registry generation.
    fn tool_state_generation(&self) -> u64;

    /// Snapshot of the tool registry at the current generation.
    fn export_tool_state(&self) -> crate::ToolState;

    /// Namespace-filtered export, as a plugin-facing handle sees it, in the
    /// formats the source's admission recorded.
    fn export_plugin_state(&self) -> Result<crate::PluginState, crate::RuntimeError>;

    /// Unfiltered capture, as the runtime commits it, in the formats the
    /// source's admission recorded (FIG-4747).
    fn capture_plugin_state(&self) -> Result<crate::PluginState, crate::RuntimeError>;

    /// `config` in the formats the source's admission recorded, or `None`
    /// when it is already written in them.
    fn committed_plugin_config(
        &self,
        config: &crate::PluginConfig,
    ) -> Result<Option<crate::PluginConfig>, crate::RuntimeError>;
}

impl<T> SessionPluginStateSource for std::sync::Arc<T>
where
    T: SessionPluginStateSource + ?Sized,
{
    fn capture_plugin_admission(
        &self,
        config: &crate::PluginConfig,
    ) -> Result<Option<std::sync::Arc<[u8]>>, crate::RuntimeError> {
        T::capture_plugin_admission(self, config)
    }

    fn tool_state_generation(&self) -> u64 {
        T::tool_state_generation(self)
    }

    fn export_tool_state(&self) -> crate::ToolState {
        T::export_tool_state(self)
    }

    fn export_plugin_state(&self) -> Result<crate::PluginState, crate::RuntimeError> {
        T::export_plugin_state(self)
    }

    fn capture_plugin_state(&self) -> Result<crate::PluginState, crate::RuntimeError> {
        T::capture_plugin_state(self)
    }

    fn committed_plugin_config(
        &self,
        config: &crate::PluginConfig,
    ) -> Result<Option<crate::PluginConfig>, crate::RuntimeError> {
        T::committed_plugin_config(self, config)
    }
}
