//! The config-change observers a deployment retired (FIG-6164): checked
//! against the plugins it installs, then declared on every plugin host its
//! core builds.

use lash_core::plugin::{PluginCallbackIdentity, PluginHost, RetiredConfigObservers};

use crate::{EmbedError, Result, RetiredConfigObserverRefusal};

/// `observers` declared retired, checked against the plugins `host` installs.
///
/// # Errors
///
/// [`EmbedError::RetiredConfigObserver`] for an identity that names no
/// lifecycle observer, or one `host` installs.
pub(super) fn checked(
    host: &PluginHost,
    observers: Vec<PluginCallbackIdentity>,
) -> Result<RetiredConfigObservers> {
    if observers.is_empty() {
        return Ok(RetiredConfigObservers::default());
    }
    let installed = host.config_change_observers()?;
    for observer in &observers {
        let refusal = if !lash_core::plugin::is_lifecycle_observer(observer) {
            RetiredConfigObserverRefusal::NotAnObserver
        } else if installed.contains(observer) {
            RetiredConfigObserverRefusal::Installed
        } else {
            continue;
        };
        return Err(EmbedError::RetiredConfigObserver {
            observer: observer.clone(),
            refusal,
        });
    }
    Ok(RetiredConfigObservers::new(observers))
}
