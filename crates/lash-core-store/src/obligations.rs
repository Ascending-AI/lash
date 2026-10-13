//! Claim obligations (S7, FIG-6074): the callbacks an actor's outstanding
//! recorded work will call, as the tokens its `requires` carries.
//!
//! One token family names every recorded callback, qualified by the registry
//! that resolves it: a session's plugins or a process's components. A node
//! serves the token of every callback its static registrations resolve,
//! without invoking any; an actor whose outstanding work names a callback
//! the node does not resolve is claimed only for its cancel. Config-format
//! tokens are a separate family and are unchanged.

use std::collections::BTreeSet;

use crate::store::plugin_writers::PluginCallbackIdentity;

/// The registry a recorded callback resolves through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RegistryKind {
    /// A session's plugins.
    Session,
    /// A process's environment components.
    Process,
}

impl RegistryKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Process => "process",
        }
    }
}

/// The one encoder of callback obligation tokens: every place a callback's
/// token is spelled, a requirement or a served registration, spells it here.
pub struct ObligationToken;

impl ObligationToken {
    /// The token of `callback`, resolved through `kind`'s registry:
    /// `callback:<kind>:<owner>/<key>`.
    #[must_use]
    pub fn for_callback(kind: RegistryKind, callback: &PluginCallbackIdentity) -> String {
        format!(
            "callback:{}:{}/{}",
            kind.as_str(),
            callback.owner,
            callback.key
        )
    }

    /// The deduplicated, sorted tokens of `callbacks`, resolved through
    /// `kind`'s registry.
    #[must_use]
    pub fn of_callbacks<'a>(
        kind: RegistryKind,
        callbacks: impl IntoIterator<Item = &'a PluginCallbackIdentity>,
    ) -> Vec<String> {
        callbacks
            .into_iter()
            .map(|callback| Self::for_callback(kind, callback))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}
