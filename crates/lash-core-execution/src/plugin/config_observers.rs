//! A session config change's observers (FIG-6164): the change records the
//! observers it is owed to, and a delivery resolves exactly those by their
//! recorded identity before any runs.
//!
//! An observer a deployment uninstalled for good is retired by name: a
//! deployment that declares it in [`RetiredConfigObservers`] serves its
//! claim token, and its delivery of a change owed to it retires that share
//! as undeliverable instead of invoking anything. A node that neither
//! installs nor retires an owed observer never claims the session to run
//! it, and a delivery that finds one unresolved delivers nothing.

use std::collections::BTreeSet;

use lash_core_store::obligations::{ObligationToken, RegistryKind};

use super::CallbackSlot;
use super::ContributionKey;
use super::ContributionKind;
use super::PluginCallbackIdentity;
use super::PluginHost;
use lash_execution_contracts::plugin_error::PluginError;

/// The session config-change observers a deployment uninstalled for good
/// and declared retired, each by its exact identity.
///
/// A retired observer's token is a session lifecycle observer's: only a
/// config change's delivery owes one, so serving it serves nothing else.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetiredConfigObservers {
    observers: BTreeSet<PluginCallbackIdentity>,
}

impl RetiredConfigObservers {
    /// The observers the deployment declares retired.
    #[must_use]
    pub fn new(observers: impl IntoIterator<Item = PluginCallbackIdentity>) -> Self {
        Self {
            observers: observers.into_iter().collect(),
        }
    }

    /// Whether `observer` is retired.
    #[must_use]
    pub fn contains(&self, observer: &PluginCallbackIdentity) -> bool {
        self.observers.contains(observer)
    }

    /// Every retired observer.
    pub fn observers(&self) -> impl Iterator<Item = &PluginCallbackIdentity> {
        self.observers.iter()
    }

    /// The claim tokens the retirement serves: each retired observer's.
    #[must_use]
    pub fn served_tokens(&self) -> BTreeSet<ObligationToken> {
        self.observers
            .iter()
            .map(|observer| ObligationToken::for_callback(RegistryKind::Session, observer))
            .collect()
    }
}

/// The typed failures of the installed observers invoked by a config-change
/// delivery, in recorded order. Explicitly retired shares require no callback
/// or retained delivery history.
#[derive(Debug, Default)]
pub struct ConfigChangeDelivery {
    pub failures: Option<PluginError>,
}

/// Whether `identity` names a session lifecycle observer.
#[must_use]
pub fn is_lifecycle_observer(identity: &PluginCallbackIdentity) -> bool {
    CallbackSlot::of(identity) == Some(CallbackSlot::RuntimeEvent)
}

impl PluginHost {
    /// This host with `retired` declared: its sessions' deliveries retire
    /// those observers' shares of a config change as undeliverable.
    #[must_use]
    pub fn with_retired_config_observers(
        mut self,
        retired: std::sync::Arc<RetiredConfigObservers>,
    ) -> Self {
        self.retired_config_observers = retired;
        self
    }

    /// The config-change observers this host declares retired.
    #[must_use]
    pub fn retired_config_observers(&self) -> &RetiredConfigObservers {
        &self.retired_config_observers
    }

    /// The session config-change observers this host installs, in
    /// registration order: what a change committed under it is owed to.
    /// Read from its registrations; nothing is built or invoked.
    ///
    /// # Errors
    ///
    /// A plugin's recipe that does not build.
    pub fn config_change_observers(&self) -> Result<Vec<PluginCallbackIdentity>, PluginError> {
        Ok(self
            .installed_inventory()?
            .entries
            .into_iter()
            .filter(|entry| {
                entry.registry == RegistryKind::Session
                    && entry.kind == ContributionKind::Callback(CallbackSlot::RuntimeEvent)
            })
            .filter_map(|entry| match entry.key {
                ContributionKey::Callback(identity) => Some(identity),
                _ => None,
            })
            .collect())
    }
}
