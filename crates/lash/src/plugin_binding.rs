use crate::support::{Arc, PluginFactory};

/// Typed app-facing activation for an ordinary Lash plugin.
///
/// A binding is not a second plugin implementation model. It gives embed hosts
/// typed session configuration while still building a normal [`PluginFactory`]
/// whose session plugin registers capabilities through
/// `lash_core::facade_support::PluginRegistrar`.
pub trait PluginBinding: Send + Sync + 'static {
    /// Stable identifier used to register and address this plugin binding.
    const ID: &'static str;
    /// Session-scoped configuration used to construct the plugin.
    type SessionConfig: Clone + Send + Sync + 'static;
    /// Creates the plugin factory for this binding configuration.
    fn factory(config: &Self::SessionConfig) -> Arc<dyn PluginFactory>;
}
