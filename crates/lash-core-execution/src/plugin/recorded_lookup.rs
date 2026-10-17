//! The one lookup of recorded callback identities (FIG-6089): a recorded
//! plan names its callbacks by identity, and each resolves against the
//! materialization's installed registrations or is refused typed, as
//! [`RecordedCallbackUnavailable`] naming every callback installed. The
//! slot-specific executors run what it resolves.

use super::PluginCallbackIdentity;
use super::RecordedCallbackUnavailable;
use super::registrar::RegisteredHook;
use lash_execution_contracts::plugin_error::PluginError;

/// The installed callbacks recorded identities resolve against.
#[derive(Clone, Copy)]
pub(crate) struct RecordedLookup<'a> {
    installed: &'a [PluginCallbackIdentity],
}

impl<'a> RecordedLookup<'a> {
    pub(crate) fn new(installed: &'a [PluginCallbackIdentity]) -> Self {
        Self { installed }
    }

    /// The registration among `registered` installed as `recorded`.
    ///
    /// # Errors
    ///
    /// [`RecordedCallbackUnavailable`] when none is.
    pub(crate) fn resolve<'h, H: 'h>(
        &self,
        recorded: &PluginCallbackIdentity,
        registered: impl IntoIterator<Item = &'h RegisteredHook<H>>,
    ) -> Result<&'h RegisteredHook<H>, PluginError> {
        registered
            .into_iter()
            .find(|registered| registered.identity == *recorded)
            .ok_or_else(|| self.unavailable(recorded))
    }

    /// Every one of `recorded`, in order, resolved among `registered`
    /// before any runs.
    ///
    /// # Errors
    ///
    /// [`RecordedCallbackUnavailable`] for the first that is not installed.
    pub(crate) fn resolve_all<'h, H: 'h>(
        &self,
        recorded: &[PluginCallbackIdentity],
        registered: &'h [RegisteredHook<H>],
    ) -> Result<Vec<&'h RegisteredHook<H>>, PluginError> {
        recorded
            .iter()
            .map(|callback| self.resolve(callback, registered))
            .collect()
    }

    /// The typed refusal of `recorded`, which no installed registration
    /// resolves.
    pub(crate) fn unavailable(&self, recorded: &PluginCallbackIdentity) -> PluginError {
        PluginError::Runtime(
            RecordedCallbackUnavailable {
                recorded: recorded.clone(),
                available: self.installed.to_vec(),
            }
            .into_runtime_error(),
        )
    }
}
