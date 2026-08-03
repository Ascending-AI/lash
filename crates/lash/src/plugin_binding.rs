use crate::support::*;

/// Typed app-facing activation for an ordinary Lash plugin.
///
/// A binding is not a second plugin implementation model. It gives embed hosts
/// typed session configuration and typed per-turn input while still building
/// a normal [`PluginFactory`] whose session plugin registers capabilities
/// through `lash_core::facade_support::PluginRegistrar`.
///
/// Plugin crates should expose a small domain extension trait over
/// [`TurnBuilder`] for their turn input instead of asking app route code to
/// call [`TurnBuilder::with_plugin_input`] directly:
///
/// ```ignore
/// trait ToneTurnExt {
///     fn with_tone(self, tone: Tone) -> Self;
/// }
///
/// impl ToneTurnExt for lash::TurnBuilder {
///     fn with_tone(self, tone: Tone) -> Self {
///         self.with_plugin_input::<TonePlugin>(ToneInput { tone })
///     }
/// }
/// ```
pub trait PluginBinding: Send + Sync + 'static {
    const ID: &'static str;
    type SessionConfig: Clone + Send + Sync + 'static;
    type Input: Clone + Send + Sync + 'static;

    fn factory(config: &Self::SessionConfig) -> Arc<dyn PluginFactory>;

    fn requires_turn_input(_config: &Self::SessionConfig) -> bool {
        false
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ActivePluginBinding {
    pub(crate) id: &'static str,
    pub(crate) requires_turn_input: bool,
}
